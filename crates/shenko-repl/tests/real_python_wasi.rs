//! Live R079 proof: bind the real `python.wasm` (CPython WASI build) and run
//! actual cells through the `python-wasi` kernel — not a fixture module.
//! Gated on `SHENKO_REAL_PYTHON_WASM` pointing at a dir containing
//! `python.wasm` and `lib/python3.15/` stdlib.

use anyhow::Result;
use shenko_repl::*;
use shenko_wasm::{MountMode, MountSpec};
use std::path::{Path, PathBuf};

fn artifact_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SHENKO_REAL_PYTHON_WASM").map(PathBuf::from)?;
    let module = dir.join("python.wasm");
    let stdlib = dir.join("lib/python3.15");
    if module.is_file() && stdlib.is_dir() { Some(dir) } else { None }
}

fn python_kernel(dir: &Path) -> KernelProfile {
    KernelProfile {
        kernel_id: "python-wasi".into(),
        runtime: ReplRuntime::CPythonWasi,
        runtime_label: "cpython-3.15 wasi-sdk-33 preview1".into(),
        backend: BackendKind::WasiPreview1,
        module: ModuleSource::ModulePath(dir.join("python.wasm")),
        argv: vec!["python.wasm".into()],
        env: vec![
            // The driver script's own dir becomes sys.path[0]; pin the stdlib
            // explicitly so `import re` (non-frozen) resolves to /lib/python3.15.
            ("PYTHONPATH".into(), "/lib/python3.15".into()),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ],
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile {
            guest_path: "/home/state.pkl".into(),
        },
        extra_mounts: vec![
            MountSpec::new(dir.join("lib"), "/lib", MountMode::ReadOnly, false, false),
        ],
        dependency_lock: vec![],
        driver: KernelDriver::PythonCellV1,
    }
}

fn cell(source: &str) -> CellRequest {
    CellRequest {
        source: source.as_bytes().to_vec(),
        cell_kind: CellKind::Pure,
        labels: Default::default(),
    }
}

#[test]
fn probe_guest_sys_path() -> Result<()> {
    let Some(dir) = artifact_dir() else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM artifact dir not set/absent");
        return Ok(());
    };
    // One-shot (no driver): run `python -c` so the guest itself prints
    // sys.path and attempts the failing imports directly.
    let kernel = KernelProfile {
        kernel_id: "python-wasi-probe".into(),
        runtime: ReplRuntime::CPythonWasi,
        runtime_label: "cpython-3.15 wasi-sdk-33 preview1".into(),
        backend: BackendKind::WasiPreview1,
        module: ModuleSource::ModulePath(dir.join("python.wasm")),
        argv: vec![
            "python.wasm".into(),
            "-c".into(),
            "import sys\nprint('PATH', sys.path)\nimport os\nL=os.listdir('/lib/python3.15')\nprint('N', len(L))\nprint('RLIKE', [x for x in L if 're' in x.lower()])\nprint('HASRE', 're' in L)\nprint('HASRE2', 're' in os.listdir('/lib/python3.15'))\nimport os.path\nprint('ISPKG', os.path.isdir('/lib/python3.15/re'))\nimport re\nprint('REOK', re.__file__)".into(),
        ],
        env: vec![
            ("PYTHONPATH".into(), "/lib/python3.15".into()),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ],
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile { guest_path: "/home/state.bin".into() },
        extra_mounts: vec![MountSpec::new(dir.join("lib"), "/lib", MountMode::ReadOnly, false, false)],
        dependency_lock: vec![],
        driver: KernelDriver::None,
    };
    let root = std::env::temp_dir().join(format!("shenko-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(CreateSession {
        kernel,
        bind: TaskBinding { task: None, worker: None, scope: Some("probe".into()) },
        limits: ResourceLimits::default(),
        preview_bytes: 0,
    })?;
    let r = session.execute(cell(""))?;
    eprintln!("OUT={:?}\nERR={:?}\nOUTCOME={:?}", r.stdout.text, r.stderr.text, r.record.outcome);
    std::fs::remove_dir_all(&root).ok();
    Ok(())
}

/// R082 on a real artifact: a recovered session replays Pure cells to rebuild
/// interpreter state — the guest variable set before drop is readable after
/// recover() with no re-execution of a DeclaredEffect.
#[test]
fn real_python_wasm_recovers_state_by_pure_cell_replay() -> Result<()> {
    let Some(dir) = artifact_dir() else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM artifact dir not set/absent");
        return Ok(());
    };
    let root = std::env::temp_dir()
        .join(format!("shenko-repl-recover-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let sessions = root.join("sessions");
    let provider = WasiReplProvider::new(sessions.clone());
    let mut first = provider.create(CreateSession {
        kernel: python_kernel(&dir),
        bind: TaskBinding { task: None, worker: None, scope: Some("recover".into()) },
        limits: ResourceLimits::default(),
        preview_bytes: 0,
    })?;
    let r = first.execute(cell("mark = 'persisted-42'\nprint('set', mark)"))?;
    assert_eq!(r.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "set cell failed: {:?}", r.stderr.text);
    let session_dir = first.session_dir().to_path_buf();
    drop(first);

    let (mut recovered, _report) = WasiReplProvider::recover(&session_dir, 0)?;
    // The recovered session must replay the Pure cell so `mark` exists again.
    let r = recovered.execute(cell("print('read', mark)"))?;
    assert_eq!(r.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "recovered cell failed: {:?}", r.stderr.text);
    assert!(r.stdout.text.contains("persisted-42"),
        "state not rebuilt by replay: stdout={:?}", r.stdout.text);

    std::fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn real_python_wasm_executes_cells_with_stdlib_and_state() -> Result<()> {
    let Some(dir) = artifact_dir() else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM artifact dir not set/absent");
        return Ok(());
    };
    let root = std::env::temp_dir()
        .join(format!("shenko-repl-realpy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let sessions = root.join("sessions");
    let provider = WasiReplProvider::new(sessions);
    let mut session = provider.create(CreateSession {
        kernel: python_kernel(&dir),
        bind: TaskBinding { task: None, worker: None, scope: Some("realpy".into()) },
        limits: ResourceLimits::default(),
        preview_bytes: 0,
    })?;

    let r1 = session.execute(cell(
        "import json, math\nx = math.factorial(10)\nprint(json.dumps({'f': x}))",
    ))?;
    assert_eq!(r1.record.seq, 1);
    assert_eq!(r1.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "cell 1 failed: stderr={:?}", r1.stderr.text);
    assert!(r1.stdout.text.contains("3628800"), "stdout={:?}", r1.stdout.text);

    // Second cell reads the persisted `x` — proves driver state survived a
    // fresh guest process, not just in-process globals.
    let r2 = session.execute(cell("print('carry', x)"))?;
    assert_eq!(r2.stdout.text.trim(), "carry 3628800",
        "state did not persist across guest processes: stdout={:?} stderr={:?}",
        r2.stdout.text, r2.stderr.text);

    std::fs::remove_dir_all(&root).ok();
    Ok(())
}

/// R083 on a real artifact: install a real pure-Python wheel into a candidate
/// environment through install_dependencies, then import it in a fresh cell.
/// Gated on the wheel path via SHENKO_REAL_PYTHON_WHEEL.
#[test]
fn real_python_wasm_installs_wheel_then_imports_it() -> Result<()> {
    let Some(dir) = artifact_dir() else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM artifact dir not set/absent");
        return Ok(());
    };
    let Some(wheel) = std::env::var_os("SHENKO_REAL_PYTHON_WHEEL").map(PathBuf::from) else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WHEEL not set");
        return Ok(());
    };
    let root = std::env::temp_dir()
        .join(format!("shenko-repl-wheel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(CreateSession {
        kernel: python_kernel(&dir),
        bind: TaskBinding { task: None, worker: None, scope: Some("wheel".into()) },
        limits: ResourceLimits::default(),
        preview_bytes: 0,
    })?;

    // Run a cell so the driver reports its compatible wheel tags before we
    // ask install_dependencies to admit a wheel against them.
    let warmup = session.execute(cell("print('ready')"))?;
    assert_eq!(warmup.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "warmup failed: {:?}", warmup.stderr.text);

    let outcome = session.install_dependencies(InstallRequest {
        wheels: vec![wheel],
        packages: vec![],
    })?;
    let InstallOutcome::Installed { installed, environment_generation, .. } = outcome else {
        panic!("wheel install refused/unsupported: {outcome:?}");
    };
    assert!(installed.iter().any(|s| s.starts_with("six=")), "installed={installed:?}");
    eprintln!("installed six into generation {environment_generation}");

    // Fresh cell on the committed generation must import the installed wheel.
    let r = session.execute(cell("import six\nprint('SIX', six.__version__)"))?;
    assert_eq!(r.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "import six failed: stderr={:?}", r.stderr.text);
    assert!(r.stdout.text.contains("1.17.0"), "stdout={:?}", r.stdout.text);

    std::fs::remove_dir_all(&root).ok();
    Ok(())
}
