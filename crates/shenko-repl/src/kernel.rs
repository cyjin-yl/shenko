//! Kernel profiles: what binds a real interpreter module to the WASI backend
//! and how the named §19.1 runtimes report their actual availability.

use crate::contract::{BackendKind, ReplRuntime};
use serde::{Deserialize, Serialize};
use shenko_wasm::{MountMode, MountSpec};
use std::path::PathBuf;

/// Where the interpreter command module comes from. The path is re-read and
/// re-hashed for every cell, so a swapped file fails binding instead of
/// silently executing a different interpreter than the one `create` saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSource {
    /// Host path to a WASI preview1 command module (`.wasm` binary or `.wat`).
    ModulePath(PathBuf),
}

/// Guest-side persistence convention (§19.1 cell-model adapter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatePersistence {
    /// The kernel keeps interpreter state inside a guest file under its work
    /// directory (e.g. an interpreter started with a state-file flag). The
    /// named path is guest-visible; reset removes it and nothing else.
    GuestFile { guest_path: String },
    /// The kernel keeps no state between cells at all.
    None,
}

/// One dependency entry in an environment lock (R083 surface; recording only —
/// no installer exists yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSpec {
    pub name: String,
    /// Pinned version/url the caller resolved; free-form on purpose.
    pub pinned: String,
}

/// A real kernel binding for the WASI preview1 backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelProfile {
    /// Stable kernel name, e.g. "python-wasi-3.13".
    pub kernel_id: String,
    /// Actual interpreter family; drives the honest capability report.
    pub runtime: ReplRuntime,
    /// Free-form runtime/ABI label, e.g. "cpython 3.13 wasi-sdk-24 preview1".
    pub runtime_label: String,
    pub backend: BackendKind,
    pub module: ModuleSource,
    /// argv[0] plus any fixed arguments that start the cell protocol
    /// (e.g. `["python.wasm", "-i-state", "/work/state.pickle"]`). The cell
    /// source itself always travels on stdin, never through argv.
    pub argv: Vec<String>,
    /// Exact environment for every cell; the guest inherits nothing.
    pub env: Vec<(String, String)>,
    /// Guest path the kernel treats as its working directory. Must equal the
    /// `/home` preopen or a declared extra mount's guest path.
    pub workdir_guest: String,
    /// How this kernel keeps interpreter state across cells, if at all.
    pub persistence: StatePersistence,
    /// Extra mounts beyond `/home`, each validated by `shenko-wasm`.
    pub extra_mounts: Vec<MountSpec>,
    /// Dependency lock the environment was created with (R083 record).
    pub dependency_lock: Vec<PackageSpec>,
}

/// Availability of a named runtime target (§19.1 capability discovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelAvailability {
    /// The WASI backend can run this runtime once a real module is bound via
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

/// The named §19.1 targets with their real binding state in this build.
/// Node/Bun stay named-but-unavailable; QuickJS-style JS is bindable only as
/// itself, never as Node or Bun.
pub fn named_targets() -> Vec<KernelTarget> {
    vec![
        KernelTarget {
            name: "python-wasi".into(),
            runtime: ReplRuntime::CPythonWasi,
            abi: "wasi-preview1".into(),
            availability: KernelAvailability::Bindable {
                via: BackendKind::WasiPreview1,
            },
            note: "CPython WASI route (§19); bind a real `python.wasm` module path"
                .into(),
        },
        KernelTarget {
            name: "pyodide".into(),
            runtime: ReplRuntime::Pyodide,
            abi: "emscripten-js-host".into(),
            availability: KernelAvailability::Unavailable {
                reason: "Pyodide needs an Emscripten/JS-host route the WASI backend does not provide (§19 profile split)".into(),
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
            note: "JS-in-WASM only; does not provide Node or Bun APIs".into(),
        },
        KernelTarget {
            name: "node-wasm".into(),
            runtime: ReplRuntime::NodeWasm,
            abi: "unverified".into(),
            availability: KernelAvailability::Unavailable {
                reason: "no verified upstream Node WASM target; Node executing WASM is not Node running on WASM (§19/R081)".into(),
            },
            note: "native/SSH Node kernels may bind separately".into(),
        },
        KernelTarget {
            name: "bun-wasm".into(),
            runtime: ReplRuntime::BunWasm,
            abi: "unverified".into(),
            availability: KernelAvailability::Unavailable {
                reason: "no verified Bun WASM target; Bun executing WASM is not Bun running on WASM (§19/R080)".into(),
            },
            note: String::new(),
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

/// Mode sanity for an extra mount requested in a kernel profile: COW is
/// unimplemented upstream and must fail here too rather than degrade.
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
    if !profile.workdir_guest.starts_with('/') {
        bail!("REPL_KERNEL_INVALID: workdir_guest must be absolute");
    }
    if let StatePersistence::GuestFile { guest_path } = &profile.persistence {
        if !guest_path.starts_with(&profile.workdir_guest) {
            bail!(
                "REPL_KERNEL_INVALID: state file {} is outside workdir {}",
                guest_path,
                profile.workdir_guest
            );
        }
    }
    for mount in &profile.extra_mounts {
        if mount.mode == MountMode::CopyOnWrite {
            bail!("REPL_KERNEL_INVALID: COW mounts are unimplemented upstream");
        }
    }
    Ok(())
}
