//! Real kernel adapters for the named §19.1 runtimes: embedded, hash-pinned
//! cell drivers plus ready-made `KernelProfile`s for each actual interpreter
//! route (WASM and native). A profile binds a *specific* interpreter artifact
//! by host path; the session re-reads and re-hashes that artifact for every
//! cell, so a swapped interpreter fails closed instead of silently executing
//! a different runtime than the one `create` admitted.
//!
//! Driver assets are embedded in this crate (versioned, content-hashed) and
//! materialized into the session's private `/home/.shenko/` directory at
//! create/fork time. A driver turns the raw "cell source on stdin" protocol
//! into a real interpreter loop with serialized state, which is what lifts a
//! kernel from R0 (work record) to R1 (explicitly serialized objects). R2
//! (resumable VM state) is not claimed anywhere in this crate.

use crate::contract::{BackendKind, ReplRuntime};
use crate::kernel::{KernelDriver, KernelProfile, ModuleSource, StatePersistence};
use shenko_wasm::{MountMode, MountSpec};
use std::path::{Path, PathBuf};

/// Guest-visible location of the driver inside the session home preopen.
pub const DRIVER_GUEST_DIR: &str = "/home/.shenko";

/// Python cell driver: runs on CPython WASI (preview1) and native CPython
/// 3.11+. Cell source arrives on stdin; state is a pickle of the cell global
/// namespace at the kernel-declared state path; non-serializable bindings are
/// dropped and reported by name in the trailer report.
pub const PYTHON_CELL_DRIVER: &[u8] = include_bytes!("drivers/python_cell_driver.py");
pub const PYTHON_CELL_DRIVER_NAME: &str = "python_cell_driver.py";

/// QuickJS cell driver: requires a QuickJS WASI build that exposes the `std`
/// module (`std.in`/`std.out`/`std.open`/`std.evalScript`, qjs-shell style).
/// State is JSON at the kernel-declared state path.
pub const QUICKJS_CELL_DRIVER: &[u8] = include_bytes!("drivers/quickjs_cell_driver.js");
pub const QUICKJS_CELL_DRIVER_NAME: &str = "quickjs_cell_driver.js";

/// JS cell driver for native Node (>= 18, ESM) and Bun (>= 1). State is JSON;
/// cells execute as a temporary ESM file under the session env directory so
/// `node_modules` installed by `install_dependencies` resolve naturally.
pub const JS_CELL_DRIVER: &[u8] = include_bytes!("drivers/js_cell_driver.mjs");
pub const JS_CELL_DRIVER_NAME: &str = "js_cell_driver.mjs";

/// Kernel that runs CPython compiled for WASI preview1 (R079 WASI route).
///
/// * `module` — host path to the real `python.wasm` (or a `.wat` text module).
/// * `stdlib` — optional host directory with the CPython stdlib `.py` files,
///   mounted read-only at `/lib` and exposed to the guest as `PYTHONPATH`.
///
/// The session materializes argv `[python.wasm, /home/.shenko/drivers/
/// python_cell_driver.py]` and the live cell protocol carries state, cell
/// streams and the active environment directory per cell on the framed stdin
/// header; the interpreter state is a pickle file at `state_guest_path`
/// (R1, serializable objects only).
pub fn python_wasi(
    kernel_id: impl Into<String>,
    module: impl Into<PathBuf>,
    stdlib: Option<PathBuf>,
) -> KernelProfile {
    let module = module.into();
    let state_guest_path = "/home/state.pkl".to_string();
    let argv = vec![guest_module_argv0(&module)];
    let mut env = vec![
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ("PYTHONUNBUFFERED".into(), "1".into()),
    ];
    let mut extra_mounts = Vec::new();
    if let Some(stdlib) = stdlib {
        env.push(("PYTHONPATH".into(), "/lib".into()));
        extra_mounts.push(MountSpec::new(
            stdlib,
            "/lib",
            MountMode::ReadOnly,
            false,
            false,
        ));
    }
    KernelProfile {
        kernel_id: kernel_id.into(),
        runtime: ReplRuntime::CPythonWasi,
        runtime_label: "cpython wasi-preview1 (driver python-cell-v1)".into(),
        backend: BackendKind::WasiPreview1,
        module: ModuleSource::ModulePath(module),
        argv,
        env,
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile { guest_path: state_guest_path },
        extra_mounts,
        dependency_lock: Vec::new(),
        driver: KernelDriver::PythonCellV1,
    }
}

/// Kernel that runs a QuickJS-for-WASI build (JS-in-WASM, explicitly *not*
/// Node or Bun). Requires the bound module to provide the qjs `std` module.
pub fn quickjs_wasi(kernel_id: impl Into<String>, module: impl Into<PathBuf>) -> KernelProfile {
    let module = module.into();
    let state_guest_path = "/home/state.json".to_string();
    let argv = vec![guest_module_argv0(&module)];
    KernelProfile {
        kernel_id: kernel_id.into(),
        runtime: ReplRuntime::JsWasm,
        runtime_label: "quickjs wasi-preview1 std-module (driver quickjs-cell-v1)".into(),
        backend: BackendKind::WasiPreview1,
        module: ModuleSource::ModulePath(module),
        argv,
        env: Vec::new(),
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile { guest_path: state_guest_path },
        extra_mounts: Vec::new(),
        dependency_lock: Vec::new(),
        driver: KernelDriver::QuickJsCellV1,
    }
}

/// Kernel that runs a native CPython on the host (an *additional* native
/// route; it does not satisfy the Python-in-WASM target). Wheels installed by
/// `install_dependencies` land in the session env and extend `sys.path`.
pub fn python_native(kernel_id: impl Into<String>, executable: impl Into<PathBuf>) -> KernelProfile {
    let state_guest_path = "/home/state.pkl".to_string();
    KernelProfile {
        kernel_id: kernel_id.into(),
        runtime: ReplRuntime::Other,
        runtime_label: "cpython 3.11+ native-host (driver python-cell-v1); not a WASM target".into(),
        backend: BackendKind::NativeHost,
        module: ModuleSource::ModulePath(executable.into()),
        argv: Vec::new(), // session materializes: [python, driver, state]
        env: vec![
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            ("PYTHONUNBUFFERED".into(), "1".into()),
        ],
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile { guest_path: state_guest_path },
        extra_mounts: Vec::new(),
        dependency_lock: Vec::new(),
        driver: KernelDriver::PythonCellV1,
    }
}

/// Kernel that runs a native Node.js on the host (additional native route;
/// Node-on-WASM stays a separate named-but-unavailable target). npm packages
/// installed by `install_dependencies` resolve from the session env.
pub fn node_native(kernel_id: impl Into<String>, executable: impl Into<PathBuf>) -> KernelProfile {
    let state_guest_path = "/home/state.json".to_string();
    KernelProfile {
        kernel_id: kernel_id.into(),
        runtime: ReplRuntime::NodeWasm,
        runtime_label: "node native-host (driver js-cell-v1); not Node-on-WASM".into(),
        backend: BackendKind::NativeHost,
        module: ModuleSource::ModulePath(executable.into()),
        argv: Vec::new(),
        env: Vec::new(),
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile { guest_path: state_guest_path },
        extra_mounts: Vec::new(),
        dependency_lock: Vec::new(),
        driver: KernelDriver::JsCellV1,
    }
}

/// Kernel that runs a native Bun on the host (additional native route;
/// Bun-on-WASM stays a separate named-but-unavailable target).
pub fn bun_native(kernel_id: impl Into<String>, executable: impl Into<PathBuf>) -> KernelProfile {
    let state_guest_path = "/home/state.json".to_string();
    KernelProfile {
        kernel_id: kernel_id.into(),
        runtime: ReplRuntime::BunWasm,
        runtime_label: "bun native-host (driver js-cell-v1); not Bun-on-WASM".into(),
        backend: BackendKind::NativeHost,
        module: ModuleSource::ModulePath(executable.into()),
        argv: Vec::new(),
        env: Vec::new(),
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile { guest_path: state_guest_path },
        extra_mounts: Vec::new(),
        dependency_lock: Vec::new(),
        driver: KernelDriver::JsCellV1,
    }
}

/// argv[0] the guest should see for a module path: the conventional
/// interpreter command name rather than a host absolute path.
fn guest_module_argv0(module: &Path) -> String {
    module
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "kernel.wasm".into())
}
