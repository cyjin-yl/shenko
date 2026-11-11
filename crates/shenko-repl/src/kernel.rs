//! Kernel profiles: what binds a real interpreter module to a backend and
//! how the named docs/specs/repl.md#spec-19-1 runtimes report their actual availability.

use crate::contract::{BackendKind, ReplRuntime, RecoveryLevel};
use serde::{Deserialize, Serialize};
use shenko_wasm::MountSpec;
use std::path::{Component, Path, PathBuf};

/// Where the interpreter command module comes from. The path is re-read and
/// re-hashed for every cell, so a swapped file fails binding instead of
/// silently executing a different interpreter than the one `create` saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSource {
    /// Host path to the interpreter: a WASI preview1 command module (`.wasm`
    /// binary or `.wat`) for the WASM backend, or an executable path for the
    /// native host backend.
    ModulePath(PathBuf),
}

/// Guest-side persistence convention (docs/specs/repl.md#spec-19-1 cell-model adapter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StatePersistence {
    /// The kernel keeps serialized interpreter state inside a guest file
    /// under a writable preopen (the driver state file). Its most specific
    /// covering preopen owns the path; reset removes that file only.
    GuestFile { guest_path: String },
    /// The kernel keeps no state between cells at all.
    None,
}

/// Embedded, versioned cell driver a kernel runs. The driver asset ships in
/// this crate (content-hashed), is materialized into the session's private
/// `/home/.shenko/` tree at create/fork, and re-verified on reopen and before
/// every cell. A driver is what turns the raw one-shot cell protocol into a
/// real long-lived interpreter session with serialized state (R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelDriver {
    /// No driver: raw one-shot protocol, one process per cell, R0 only.
    #[default]
    None,
    /// Python cell driver v1 (CPython WASI or native CPython). Pickle state.
    PythonCellV1,
    /// QuickJS cell driver v1; requires a qjs WASI build with the `std`
    /// module. JSON state.
    QuickJsCellV1,
    /// JS cell driver v1 for native Node/Bun. JSON state.
    JsCellV1,
}

impl KernelDriver {
    /// Stable driver identity recorded in the manifest: id + content blake3.
    pub fn identity(&self) -> Option<String> {
        self.asset().map(|asset| format!("{}@{}", asset.id, blake3::hash(asset.bytes).to_hex()))
    }

    pub fn blake3(&self) -> Option<String> {
        self.asset().map(|asset| blake3::hash(asset.bytes).to_hex().to_string())
    }

    /// The embedded driver asset, if any.
    pub fn asset(&self) -> Option<DriverAsset> {
        match self {
            Self::None => None,
            Self::PythonCellV1 => Some(DriverAsset {
                id: "python-cell-driver/v1",
                file_name: crate::adapters::PYTHON_CELL_DRIVER_NAME,
                media_type: "text/x-python",
                bytes: crate::adapters::PYTHON_CELL_DRIVER,
            }),
            Self::QuickJsCellV1 => Some(DriverAsset {
                id: "quickjs-cell-driver/v1",
                file_name: crate::adapters::QUICKJS_CELL_DRIVER_NAME,
                media_type: "text/javascript",
                bytes: crate::adapters::QUICKJS_CELL_DRIVER,
            }),
            Self::JsCellV1 => Some(DriverAsset {
                id: "js-cell-driver/v1",
                file_name: crate::adapters::JS_CELL_DRIVER_NAME,
                media_type: "text/javascript",
                bytes: crate::adapters::JS_CELL_DRIVER,
            }),
        }
    }
}

/// An embedded driver file.
pub struct DriverAsset {
    pub id: &'static str,
    pub file_name: &'static str,
    pub media_type: &'static str,
    pub bytes: &'static [u8],
}

/// One dependency entry in an environment lock (R083 record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSpec {
    pub name: String,
    /// Pinned version/url the caller resolved; free-form on purpose.
    pub pinned: String,
}

/// A real kernel binding for a backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelProfile {
    /// Stable kernel name, e.g. "python-wasi-3.13".
    pub kernel_id: String,
    /// Actual interpreter family; drives the honest capability report.
    pub runtime: ReplRuntime,
    /// Free-form runtime/ABI label, e.g. "cpython 3.13 wasi-sdk-24 preview1".
    pub runtime_label: String,
    pub backend: BackendKind,
    pub module: ModuleSource,
    /// argv[0] plus any fixed arguments that start the cell protocol.
    /// Driver kernels append the driver path; native kernels may leave this
    /// empty and let the session materialize `[executable, driver]`. The cell
    /// source itself travels on the session protocol, never through argv.
    pub argv: Vec<String>,
    /// Exact base environment for every cell; the guest inherits nothing.
    /// The session adds protocol paths (state, per-cell dirs) per cell.
    pub env: Vec<(String, String)>,
    /// Guest path the kernel treats as its working directory. Must equal the
    /// `/home` preopen, a declared extra mount's guest path, or the private
    /// workdir preopen created by this session.
    pub workdir_guest: String,
    /// How this kernel keeps interpreter state across cells, if at all.
    pub persistence: StatePersistence,
    /// Extra mounts beyond `/home`, each validated by `shenko-wasm`.
    pub extra_mounts: Vec<MountSpec>,
    /// Dependency lock the environment was created with (R083 record).
    pub dependency_lock: Vec<PackageSpec>,
    /// Embedded cell driver, if this kernel runs one.
    #[serde(default)]
    pub driver: KernelDriver,
}

impl KernelProfile {
    /// The session model this profile actually delivers.
    pub fn session_model(&self) -> crate::contract::SessionModel {
        match self.driver {
            KernelDriver::None => crate::contract::SessionModel::OneShot,
            _ => crate::contract::SessionModel::LiveProcess,
        }
    }

    /// Honest recovery ceiling for this profile. R2 is never claimed; R1
    /// requires a driver plus a declared writable state file.
    pub fn recovery_level(&self) -> RecoveryLevel {
        match (self.driver, &self.persistence) {
            (KernelDriver::None, _) => RecoveryLevel::R0WorkRecord,
            (_, StatePersistence::GuestFile { .. }) => RecoveryLevel::R1ObjectCheckpoint,
            (_, StatePersistence::None) => RecoveryLevel::R0WorkRecord,
        }
    }
}

/// Availability of a named runtime target (docs/specs/repl.md#spec-19-1 capability discovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelAvailability {
    /// A backend can run this runtime once a real module is bound via
    /// `ModuleSource::ModulePath`.
    Bindable { via: BackendKind },
    /// No usable binding path exists in this build.
    Unavailable { reason: String },
}

/// A named runtime target as it stands — including named-but-unavailable
/// entries, which must never report as satisfied by another runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelTarget {
    pub name: String,
    pub runtime: ReplRuntime,
    pub abi: String,
    pub availability: KernelAvailability,
    pub note: String,
}

/// The named docs/specs/repl.md#spec-19-1 targets with their real binding state in this build.
/// WASM targets stay the obligation: python-wasi and quickjs-wasm are
/// bindable to a real interpreter module, node-wasm/bun-wasm stay
/// named-but-unavailable (no verified upstream Node/Bun-on-WASM target),
/// pyodide needs an Emscripten route this crate does not provide. Native
/// host kernels are additional real routes and never satisfy a WASM target.
pub fn named_targets() -> Vec<KernelTarget> {
    vec![
        KernelTarget {
            name: "python-wasi".into(),
            runtime: ReplRuntime::CPythonWasi,
            abi: "wasi-preview1".into(),
            availability: KernelAvailability::Bindable {
                via: BackendKind::WasiPreview1,
            },
            note: "CPython WASI route (docs/specs/repl.md#spec-19/R079); bind a real `python.wasm` module path; live session via embedded python-cell-driver".into(),
        },
        KernelTarget {
            name: "pyodide".into(),
            runtime: ReplRuntime::Pyodide,
            abi: "emscripten-js-host".into(),
            availability: KernelAvailability::Unavailable {
                reason: "Pyodide needs an Emscripten/JS-host route the WASI backend does not provide (docs/specs/repl.md#spec-19 profile split)".into(),
            },
            note: "separate profile, not interchangeable with the WASI route".into(),
        },
        KernelTarget {
            name: "quickjs-wasm".into(),
            runtime: ReplRuntime::JsWasm,
            abi: "wasi-preview1".into(),
            availability: KernelAvailability::Bindable {
                via: BackendKind::WasiPreview1,
            },
            note: "JS-in-WASM only; requires a qjs WASI build exposing the `std` module; never provides Node or Bun APIs".into(),
        },
        KernelTarget {
            name: "node-wasm".into(),
            runtime: ReplRuntime::NodeWasm,
            abi: "unverified".into(),
            availability: KernelAvailability::Unavailable {
                reason: "no verified upstream Node WASM target; Node executing WASM is not Node running on WASM (docs/specs/repl.md#spec-19/R081)".into(),
            },
            note: "native host Node kernel is a separate additional route".into(),
        },
        KernelTarget {
            name: "bun-wasm".into(),
            runtime: ReplRuntime::BunWasm,
            abi: "unverified".into(),
            availability: KernelAvailability::Unavailable {
                reason: "no verified Bun WASM target; Bun executing WASM is not Bun running on WASM (docs/specs/repl.md#spec-19/R080)".into(),
            },
            note: "native host Bun kernel is a separate additional route".into(),
        },
        KernelTarget {
            name: "python-native-host".into(),
            runtime: ReplRuntime::Other,
            abi: "native-host".into(),
            availability: KernelAvailability::Bindable {
                via: BackendKind::NativeHost,
            },
            note: "additional native route; does not satisfy the Python-in-WASM target".into(),
        },
        KernelTarget {
            name: "node-native-host".into(),
            runtime: ReplRuntime::NodeWasm,
            abi: "native-host".into(),
            availability: KernelAvailability::Bindable {
                via: BackendKind::NativeHost,
            },
            note: "additional native route; does not satisfy the Node-on-WASM target".into(),
        },
        KernelTarget {
            name: "bun-native-host".into(),
            runtime: ReplRuntime::BunWasm,
            abi: "native-host".into(),
            availability: KernelAvailability::Bindable {
                via: BackendKind::NativeHost,
            },
            note: "additional native route; does not satisfy the Bun-on-WASM target".into(),
        },
    ]
}

/// Environment generation is the hash of the kernel identity plus the exact
/// dependency lock, so a changed lock yields a new generation without any
/// counter that could be reused (R083 record semantics).
pub fn environment_generation(kernel: &KernelProfile) -> shenko_types::ids::RuntimeGeneration {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kernel.kernel_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(kernel.runtime_label.as_bytes());
    hasher.update(b"\0driver:");
    hasher.update(kernel.driver.identity().unwrap_or_default().as_bytes());
    for package in &kernel.dependency_lock {
        hasher.update(b"\0");
        hasher.update(package.name.as_bytes());
        hasher.update(b"=");
        hasher.update(package.pinned.as_bytes());
    }
    shenko_types::ids::RuntimeGeneration::new(format!(
        "{}:{}",
        kernel.kernel_id,
        hasher.finalize().to_hex()
    ))
}

/// Profile sanity. Mount modes delegate to `shenko-wasm`'s validator: what
/// the runtime rejects (including mount modes it has not implemented) is
/// rejected here, and nothing more.
pub fn validate_profile(profile: &KernelProfile) -> anyhow::Result<()> {
    use anyhow::bail;
    if profile.kernel_id.is_empty() || profile.kernel_id.contains('\0') {
        bail!("REPL_KERNEL_INVALID: kernel_id must be non-empty and NUL-free");
    }
    match &profile.module {
        ModuleSource::ModulePath(path) => {
            if path.as_os_str().is_empty() {
                bail!("REPL_KERNEL_INVALID: module path is empty");
            }
        }
    }
    match profile.backend {
        BackendKind::WasiPreview1 | BackendKind::NativeHost => {}
        other => bail!("REPL_KERNEL_INVALID: backend {other:?} is not implemented by this crate"),
    }
    if profile.backend == BackendKind::WasiPreview1 && profile.driver == KernelDriver::JsCellV1 {
        bail!("REPL_KERNEL_INVALID: the js-cell driver requires the native host backend");
    }
    if profile.backend == BackendKind::NativeHost
        && matches!(profile.driver, KernelDriver::QuickJsCellV1)
    {
        bail!("REPL_KERNEL_INVALID: the quickjs driver requires the WASI backend");
    }
    if profile.driver != KernelDriver::None {
        // A live session reports and restores through the declared state
        // file; without one the driver protocol has no persistence target.
        if !matches!(profile.persistence, StatePersistence::GuestFile { .. }) {
            bail!("REPL_KERNEL_INVALID: driver kernels require a declared writable state file");
        }
        // The session appends the materialized driver path itself; a profile
        // that also lists it would execute the driver twice.
        if profile.argv.iter().any(|arg| arg.contains("/.shenko/drivers/")) {
            bail!("REPL_KERNEL_INVALID: argv must not predeclare the session driver path");
        }
    }
    validate_guest_path(&profile.workdir_guest)?;
    if let StatePersistence::GuestFile { guest_path } = &profile.persistence {
        // The shared session preopen list admits ownership before creation;
        // do not assume that the workdir also owns every state path below it.
        validate_guest_path(guest_path)?;
    }
    for mount in &profile.extra_mounts {
        mount.validate()?;
        validate_guest_path(&mount.guest_path)?;
    }
    Ok(())
}

/// Whether a host path is a regular file (symlinks followed) carrying an
/// execute bit. An installer resolved next to a native interpreter and the
/// interpreter artifact itself are both exec'd directly.
pub(crate) fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// A native kernel executes the bound artifact directly, so the artifact must
/// actually be executable; a regular file without an execute bit is not an
/// interpreter and is refused at admission instead of failing at first cell.
/// WASI modules are read by the runtime, where the mode is irrelevant and is
/// deliberately not invented as a requirement.
pub fn validate_module_mode(profile: &KernelProfile, module: &Path) -> anyhow::Result<()> {
    if profile.backend != BackendKind::NativeHost {
        return Ok(());
    }
    if executable_file(module) {
        return Ok(());
    }
    anyhow::bail!(
        "REPL_KERNEL_UNAVAILABLE: native interpreter {} is not an executable file",
        module.display()
    )
}

fn validate_guest_path(path: &str) -> anyhow::Result<()> {
    if !path.starts_with('/') || path.contains('\0')
        || (path != "/" && path[1..].split('/').any(|part| part.is_empty() || part == "." || part == ".."))
    {
        anyhow::bail!("REPL_KERNEL_INVALID: non-canonical guest path {path:?}");
    }
    Ok(())
}

/// Component-aware admission: `/home2` is not below `/home`.
pub(crate) fn state_relative_path(owner_guest: &str, guest_path: &str) -> anyhow::Result<PathBuf> {
    validate_guest_path(guest_path)?;
    let relative = Path::new(guest_path).strip_prefix(owner_guest)
        .map_err(|_| anyhow::anyhow!("REPL_KERNEL_INVALID: state path escapes owning preopen"))?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|part| !matches!(part, Component::Normal(_)))
    {
        anyhow::bail!("REPL_KERNEL_INVALID: state path must name a file below its owning preopen");
    }
    Ok(relative.to_path_buf())
}
