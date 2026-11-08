//! Live proof for the quickjs-wasm kernel: bind the real qjs.wasm
//! (QuickJS-NG command build) and run actual JS cells through the embedded
//! quickjs cell driver — not a fixture module. Gated on
//! SHENKO_REAL_QJS_WASM pointing at the built qjs.wasm path.

use anyhow::Result;
use shenko_repl::*;
use std::path::PathBuf;

fn artifact() -> Option<PathBuf> {
    let p = std::env::var_os("SHENKO_REAL_QJS_WASM").map(PathBuf::from)?;
    if p.is_file() { Some(p) } else { None }
}

fn qjs_kernel(module: PathBuf) -> KernelProfile {
    KernelProfile {
        kernel_id: "quickjs-wasm".into(),
        runtime: ReplRuntime::JsWasm,
        runtime_label: "quickjs-ng d7215b0 wasi-sdk-34 preview1".into(),
        backend: BackendKind::WasiPreview1,
        module: ModuleSource::ModulePath(module),
        argv: vec!["qjs.wasm".into()],
        env: vec![],
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile {
            guest_path: "/home/state.json".into(),
        },
        extra_mounts: vec![],
        dependency_lock: vec![],
        driver: KernelDriver::QuickJsCellV1,
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
fn real_qjs_wasm_executes_cells_with_state() -> Result<()> {
    let Some(module) = artifact() else {
        eprintln!("SKIP: SHENKO_REAL_QJS_WASM artifact not set/absent");
        return Ok(());
    };
    let root = std::env::temp_dir()
        .join(format!("shenko-repl-realqjs-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(CreateSession {
        kernel: qjs_kernel(module),
        bind: TaskBinding { task: None, worker: None, scope: Some("realqjs".into()) },
        limits: ResourceLimits::default(),
        preview_bytes: 0,
    })?;

    // A real JS cell: arithmetic + JSON + a global the driver must serialize
    // into state.json for the next cell's guest process.
    let r1 = session.execute(cell(
        "var prod = [2,3,5,7].reduce((a,b)=>a*b);\nprint(JSON.stringify({prod:prod}));",
    ))?;
    assert_eq!(r1.record.seq, 1);
    assert_eq!(r1.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "cell 1 failed: stderr={:?}", r1.stderr.text);
    assert!(r1.stdout.text.contains("210"), "stdout={:?}", r1.stdout.text);

    // Second cell reads the persisted `prod` — proves driver state.json
    // survived into a fresh guest process.
    let r2 = session.execute(cell("print('carry', prod)"))?;
    assert_eq!(r2.stdout.text.trim(), "carry 210",
        "state did not persist across guest processes: stdout={:?} stderr={:?}",
        r2.stdout.text, r2.stderr.text);

    std::fs::remove_dir_all(&root).ok();
    Ok(())
}
