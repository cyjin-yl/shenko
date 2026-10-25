use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::object::ObjectRef;
use shenko_wasm::{HOME_GUEST_PATH, MountMode, MountSpec, WasmRuntime};
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

pub struct PreparedHome {
    pub manifest: HomeManifest,
    pub manifest_path: PathBuf,
    /// Kept only for the bootstrap phase; the caller drops it before the main
    /// loop so merely holding a home does not keep an engine per subject step.
    pub runtime: WasmRuntime,
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
                model: Some("qwen3.8-27b".into()),
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
                if name != "home" && name != "memory" {
                    bail!(
                        "DATA_PATH_CONFLICT: unrecognized data in {}; choose a new path or import explicitly",
                        root.display()
                    );
                }
            }
        }
        for managed in ["home", "memory"] {
            let path = root.join(managed);
            if path.exists() && !path.is_dir() {
                bail!("DATA_PATH_CONFLICT: {} is not a directory", path.display());
            }
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

        if !manifest_path.exists() {
            if directory_has_entries(&home_input)? {
                bail!(
                    "DATA_PATH_CONFLICT: nonempty home {} has no installation manifest",
                    home_input.display()
                );
            }
            if directory_has_entries(&memory_input)? {
                bail!(
                    "DATA_PATH_CONFLICT: nonempty memory {} has no installation manifest; import it explicitly instead of adopting unknown data",
                    memory_input.display()
                );
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

        let mut manifest = if manifest_path.exists() {
            let bytes = fs::read(&manifest_path).map_err(|error| {
                anyhow!(
                    "SEED_CLOSURE_INCOMPLETE: read {}: {error}",
                    manifest_path.display()
                )
            })?;
            let stored: HomeManifest = serde_json::from_slice(&bytes).map_err(|error| {
                anyhow!(
                    "SEED_CLOSURE_INCOMPLETE: decode {}: {error}",
                    manifest_path.display()
                )
            })?;
            if stored.schema_version != 1 {
                bail!(
                    "SEED_CLOSURE_INCOMPLETE: unsupported home manifest schema {}",
                    stored.schema_version
                );
            }
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
            if stored.checkpoint != home_checkpoint() || stored.capabilities != home_capabilities()
            {
                bail!(
                    "SEED_CLOSURE_INCOMPLETE: stored checkpoint/capability declaration differs from this seed; migrate explicitly"
                );
            }
            stored
        } else {
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
        };

        let memory = manifest.memory_dir().to_path_buf();
        let git_marker = memory.join(".git");
        if !git_marker.exists() {
            if manifest.state == HomeState::Ready || directory_has_entries(&memory)? {
                bail!(
                    "SEED_CLOSURE_INCOMPLETE: managed Git repository is missing at {}; data preserved",
                    memory.display()
                );
            }
            run_git(&memory, &["init", "--quiet", "--initial-branch=main"])?;
        }
        verify_git_repository(&memory)?;

        let mut runtime = WasmRuntime::with_mounts(manifest.mounts.clone())?;
        let ready_file = manifest.home_dir().join(HOME_READY_FILE);
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
                    bail!(
                        "SEED_CLOSURE_INCOMPLETE: guest readiness content changed at {}; data preserved",
                        ready_file.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if manifest.state == HomeState::Ready {
                    bail!(
                        "SEED_CLOSURE_INCOMPLETE: ready home lost its guest readiness file at {}; data preserved",
                        ready_file.display()
                    );
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect guest readiness file {}", ready_file.display())
                });
            }
        }
        let exit = runtime.run_wasi_module(HOME_PROBE_WAT.as_bytes())?;
        if exit != 0 {
            bail!("SEED_CLOSURE_INCOMPLETE: home guest probe exited {exit}");
        }
        let observed = fs::read_to_string(&ready_file)
            .with_context(|| format!("host readback of {}", ready_file.display()))?;
        if observed != HOME_READY_BODY {
            bail!(
                "SEED_CLOSURE_INCOMPLETE: guest/home host readback mismatch at {}",
                ready_file.display()
            );
        }

        if manifest.state != HomeState::Ready {
            manifest.state = HomeState::Ready;
            persist_manifest(&manifest_path, &manifest)?;
        }
        Ok(PreparedHome {
            manifest,
            manifest_path,
            runtime,
        })
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
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
        CapabilityStatus::Unsupported,
        "COW requests fail explicitly; never downgraded to direct RW",
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
