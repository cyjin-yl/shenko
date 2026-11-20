//! Live R079 proof: bind the real `python.wasm` (CPython WASI build) and run
//! actual cells through the `python-wasi` kernel — not a fixture module.
//! Gated on `SHENKO_REAL_PYTHON_WASM` pointing at a dir containing
//! `python.wasm` and `lib/python3.15/` stdlib.
//! An unset artifact skips these tests; a configured invalid artifact fails.
//! The recovery proof also requires `SHENKO_REAL_PYTHON_EVIDENCE_ROOT`, a
//! writable persistent directory. Every attempt retains a fresh child world,
//! producer logs/status, original cell/checkpoint, and recovery observations.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use shenko_repl::*;
use shenko_wasm::{MountMode, MountSpec};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn artifact_dir() -> Result<Option<PathBuf>> {
    let Some(configured) = std::env::var_os("SHENKO_REAL_PYTHON_WASM") else {
        return Ok(None);
    };
    ensure!(!configured.is_empty(), "SHENKO_REAL_PYTHON_WASM is configured but empty");
    let configured = PathBuf::from(configured);
    let dir = configured.canonicalize().with_context(||
        format!("configured SHENKO_REAL_PYTHON_WASM is unavailable: {}", configured.display()))?;
    let module = dir.join("python.wasm");
    let stdlib = dir.join("lib/python3.15");
    ensure!(module.is_file(), "configured CPython WASI module is missing: {}", module.display());
    ensure!(stdlib.is_dir(), "configured CPython standard library is missing: {}", stdlib.display());
    let mut header = [0; 8];
    File::open(&module)?.read_exact(&mut header)
        .context("read configured CPython WASI module header")?;
    ensure!(header == *b"\0asm\x01\0\0\0", "configured python.wasm is not a core WebAssembly module");
    Ok(Some(dir))
}

fn python_kernel(dir: &Path) -> KernelProfile {
    KernelProfile {
        kernel_id: "python-wasi".into(),
        runtime: ReplRuntime::CPythonWasi,
        runtime_label: "cpython-3.15 wasm32-wasip1".into(),
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
    let Some(dir) = artifact_dir()? else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM is not set");
        return Ok(());
    };
    // One-shot (no driver): run `python -c` so the guest itself prints
    // sys.path and attempts the failing imports directly.
    let kernel = KernelProfile {
        kernel_id: "python-wasi-probe".into(),
        runtime: ReplRuntime::CPythonWasi,
        runtime_label: "cpython-3.15 wasm32-wasip1".into(),
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

const RECOVERY_TEST: &str =
    "real_python_wasm_restores_declared_effect_checkpoint_after_process_reopen";
const PRODUCER_ROOT_ENV: &str = "SHENKO_REAL_PYTHON_RECOVERY_PRODUCER_ROOT";
const PRODUCER_TOKEN_ENV: &str = "SHENKO_REAL_PYTHON_RECOVERY_PRODUCER_TOKEN";
const PRODUCER_TIMEOUT: Duration = Duration::from_secs(430);
const EFFECT_BYTES: &[u8] = b"python-wasi-effect-once\n";
const EFFECT_SOURCE: &str = r#"import os, sys
assert sys.implementation.name == 'cpython'
assert sys.platform == 'wasi' and sys.maxsize == 2147483647
mark = {'label': 'persisted-42', 'values': [2, 4, 6]}
with open('/witness/append.bin', 'ab') as witness:
    witness.write(b'python-wasi-effect-once\n')
    witness.flush()
    os.fsync(witness.fileno())
print('set', mark['label'], mark['values'])
"#;

#[derive(Serialize, Deserialize)]
struct EffectProducerReceipt {
    producer_pid: u32,
    session_dir: PathBuf,
    result: CellResult,
}

fn write_evidence(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = File::create_new(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// A panic or a wait error must not leave the producer (including its WASI
/// interpreter threads) running after the caller releases this test's world.
struct ProducerProcess {
    child: Child,
    reaped: bool,
}

impl Drop for ProducerProcess {
    fn drop(&mut self) {
        if !self.reaped {
            if let Err(error) = self.child.kill() {
                eprintln!("producer cleanup kill failed: {error}");
            }
            if let Err(error) = self.child.wait() {
                eprintln!("producer cleanup reap failed: {error}");
            }
        }
    }
}

fn run_effect_producer(dir: &Path, root: &Path, token: &str) -> Result<u32> {
    let executable = std::env::current_exe()?.canonicalize()?;
    write_evidence(&root.join("producer-command.json"), &serde_json::json!({
        "executable": executable,
        "executable_blake3": hash_file(&executable)?,
        "args": ["--exact", RECOVERY_TEST, "--nocapture", "--test-threads=1"],
        "artifact_dir": dir,
        "module_blake3": hash_file(&dir.join("python.wasm"))?,
        "world": root,
        "parent_pid": std::process::id(),
        "timeout_ms": PRODUCER_TIMEOUT.as_millis(),
    }))?;
    let started = Instant::now();
    let child = Command::new(&executable)
        .args(["--exact", RECOVERY_TEST, "--nocapture", "--test-threads=1"])
        .env("SHENKO_REAL_PYTHON_WASM", dir)
        .env(PRODUCER_ROOT_ENV, root)
        .env(PRODUCER_TOKEN_ENV, token)
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create_new(root.join("producer.stdout.log"))?))
        .stderr(Stdio::from(File::create_new(root.join("producer.stderr.log"))?))
        .spawn().context("start the same test executable as the effect producer")?;
    let producer_pid = child.id();
    let mut producer = ProducerProcess { child, reaped: false };
    let (status, timed_out, kill_error) = loop {
        if let Some(status) = producer.child.try_wait().context("poll effect producer")? {
            break (status, false, None);
        }
        if started.elapsed() >= PRODUCER_TIMEOUT {
            let kill_error = producer.child.kill().err().map(|error| error.to_string());
            let status = producer.child.wait().context("reap timed-out effect producer")?;
            break (status, true, kill_error);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    producer.reaped = true;
    write_evidence(&root.join("producer-status.json"), &serde_json::json!({
        "producer_pid": producer_pid,
        "status": status.to_string(),
        "exit_code": status.code(),
        "success": status.success(),
        "timed_out": timed_out,
        "kill_error": kill_error,
        "elapsed_ms": started.elapsed().as_millis(),
    }))?;
    ensure!(!timed_out, "effect producer exceeded {PRODUCER_TIMEOUT:?}; retained evidence: {}", root.display());
    ensure!(status.success(), "effect producer failed ({status}); retained logs: {}", root.display());
    let stdout = fs::read_to_string(root.join("producer.stdout.log"))?;
    ensure!(stdout.contains("1 passed; 0 failed"),
        "producer must run exactly one passing case; retained stdout: {}", root.display());
    Ok(producer_pid)
}

fn produce_declared_effect(dir: &Path, root: &Path) -> Result<()> {
    let mut kernel = python_kernel(dir);
    // This sibling preopen is outside the checkpointed /home tree, so a
    // duplicated append cannot be hidden by restoring interpreter state.
    kernel.extra_mounts.push(MountSpec::new(
        root.join("witness"), "/witness", MountMode::ReadWrite, true, false));
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(CreateSession {
        kernel,
        bind: TaskBinding { task: None, worker: None, scope: Some("effect-recovery".into()) },
        // Cold CPython WASI initialization shares the cell deadline in this fixture.
        limits: ResourceLimits { max_wall_clock_ms: Some(300_000), ..Default::default() },
        preview_bytes: 0,
    })?;
    let mut request = cell(EFFECT_SOURCE);
    request.cell_kind = CellKind::DeclaredEffect;
    let result = session.execute(request)?;
    let info = session.inspect()?;
    write_evidence(&root.join("producer-info.json"), &info)?;
    let receipt = EffectProducerReceipt {
        producer_pid: std::process::id(),
        session_dir: session.session_dir().to_path_buf(),
        result,
    };
    write_evidence(&root.join("producer.json"), &receipt)?;
    assert_eq!(receipt.result.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "declared effect failed: {:?}", receipt.result.stderr.text);
    assert_eq!(info.runtime, ReplRuntime::CPythonWasi);
    assert_eq!(info.backend, BackendKind::WasiPreview1);
    assert_eq!(info.session_model, SessionModel::LiveProcess);
    assert_eq!(info.recovery_level, RecoveryLevel::R1ObjectCheckpoint);
    assert!(info.live_process_running && info.interpreter_state_persists);
    assert_eq!(fs::read(root.join("witness/append.bin"))?, EFFECT_BYTES);
    // The test harness then returns normally and the entire producer process
    // must exit successfully before the parent is allowed to reopen anything.
    drop(session);
    Ok(())
}

/// A15/R079/R082: a real wasm32-wasip1 CPython PythonCellV1 effect produces an
/// immutable R1 checkpoint. A different host process restores its exact cell
/// identity/sequence/digest without source replay or a second external append.
#[test]
fn real_python_wasm_restores_declared_effect_checkpoint_after_process_reopen() -> Result<()> {
    let producer_root = std::env::var_os(PRODUCER_ROOT_ENV);
    let producer_token = std::env::var_os(PRODUCER_TOKEN_ENV);
    ensure!(producer_root.is_some() == producer_token.is_some(),
        "producer root and guard token must be configured together");
    let Some(dir) = artifact_dir()? else {
        ensure!(producer_root.is_none(), "a producer invocation requires the real CPython WASI artifact");
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM is not set");
        return Ok(());
    };
    if let (Some(root), Some(token)) = (producer_root, producer_token) {
        let token = token.to_str().context("producer guard token is not UTF-8")?;
        uuid::Uuid::parse_str(token).context("producer guard token is not a UUID")?;
        let root = PathBuf::from(root).canonicalize()?;
        let name = format!("declared-effect-recovery-{token}");
        ensure!(root.file_name() == Some(std::ffi::OsStr::new(&name)), "producer world/token mismatch");
        ensure!(fs::read_to_string(root.join("producer.guard"))? == token,
            "producer guard does not authorize this world");
        ensure!(!root.join("sessions").exists(), "producer world has already been used");
        // Consume the parent's one-use guard while preserving it as evidence.
        fs::rename(root.join("producer.guard"), root.join("producer.admitted"))?;
        return produce_declared_effect(&dir, &root);
    }

    let evidence_root = std::env::var_os("SHENKO_REAL_PYTHON_EVIDENCE_ROOT")
        .context("set SHENKO_REAL_PYTHON_EVIDENCE_ROOT to a writable persistent evidence directory")?;
    ensure!(!evidence_root.is_empty(), "SHENKO_REAL_PYTHON_EVIDENCE_ROOT is empty");
    let evidence_root = PathBuf::from(evidence_root);
    fs::create_dir_all(&evidence_root)?;
    let token = uuid::Uuid::new_v4().to_string();
    let root = evidence_root.canonicalize()?.join(format!("declared-effect-recovery-{token}"));
    fs::create_dir(&root)?;
    eprintln!("retaining CPython WASI effect recovery evidence: {}", root.display());
    fs::create_dir(root.join("witness"))?;
    fs::write(root.join("producer.guard"), token.as_bytes())?;
    let producer_pid = run_effect_producer(&dir, &root, &token)?;
    let produced: EffectProducerReceipt = serde_json::from_slice(&fs::read(root.join("producer.json"))?)?;
    assert_eq!(produced.producer_pid, producer_pid);
    assert_ne!(produced.producer_pid, std::process::id(), "producer must be a separate process");
    let original = &produced.result.record;
    assert_eq!(original.seq, 1);
    assert_eq!(original.cell_kind, CellKind::DeclaredEffect);
    assert_eq!(original.outcome, CellOutcome::Completed { exit_code: 0 });
    assert_eq!(original.source_blake3, hash_bytes(EFFECT_SOURCE.as_bytes()));
    assert_eq!(original.module_blake3, hash_file(&dir.join("python.wasm"))?);
    assert_eq!(produced.session_dir, root.join("sessions").join(original.session_id.to_string()));
    let driver = original.driver_report.as_ref().context("effect cell must retain its driver report")?;
    assert!(driver.ok && driver.state_verified, "driver report: {driver:?}");
    assert!(!driver.dropped.iter().any(|name| name == "mark"), "serializable mark was dropped");
    let checkpoint = original.object_checkpoint.as_ref().context("effect cell must have an immutable R1 checkpoint")?;
    let checkpoint_path = produced.session_dir.join(CELLS_DIR).join(format!("{}.state", original.cell));
    let checkpoint_bytes = fs::read(&checkpoint_path)?;
    assert!(!checkpoint_bytes.is_empty());
    assert_eq!(checkpoint.bytes, checkpoint_bytes.len() as u64);
    assert_eq!(checkpoint.bytes, driver.state_bytes);
    assert_eq!(checkpoint.blake3, hash_bytes(&checkpoint_bytes));
    let journal_path = produced.session_dir.join(JOURNAL_FILE);
    let original_journal = fs::read(&journal_path)?;
    fs::write(root.join("producer-journal.jsonl"), &original_journal)?;
    let journal_record: CellRecord = serde_json::from_slice(&original_journal)?;
    assert_eq!(serde_json::to_value(&journal_record)?, serde_json::to_value(original)?,
        "producer receipt must cite the original immutable cell record");
    let witness = root.join("witness/append.bin");
    assert_eq!(fs::read(&witness)?, EFFECT_BYTES, "effect must have happened exactly once before reopen");

    // Retain the old mutable state outside the guest's mounts. Recovery must
    // restore the original immutable checkpoint, rather than hydrate a state
    // file that happened to survive the producer process.
    let state_path = produced.session_dir.join("home/state.pkl");
    fs::rename(&state_path, root.join("producer-state.pkl"))?;
    assert_eq!(fs::read(root.join("producer-state.pkl"))?, checkpoint_bytes);
    assert!(!state_path.exists());
    let (mut recovered, report) = WasiReplProvider::recover(&produced.session_dir, 0)?;
    write_evidence(&root.join("recovery-report.json"), &report)?;
    assert_eq!(report.session_id, original.session_id);
    assert!(report.torn_tail_at.is_none());
    assert_eq!(report.entries.len(), 1);
    assert!(!report.entries.iter().any(|entry| matches!(entry.recovery, CellRecovery::Replayed { .. })),
        "live Python recovery must never replay authored source: {report:?}");
    let entry = &report.entries[0];
    assert_eq!(entry.cell, original.cell);
    assert_eq!(entry.seq, original.seq);
    assert_eq!(entry.cell_kind, CellKind::DeclaredEffect);
    match &entry.recovery {
        CellRecovery::ObjectRestored { checkpoint_blake3, dropped } => {
            assert_eq!(checkpoint_blake3, &checkpoint.blake3);
            assert_eq!(dropped, &driver.dropped);
        }
        other => bail!("declared-effect checkpoint must restore as R1 objects: {other:?}"),
    }
    let info = recovered.inspect()?;
    write_evidence(&root.join("recovered-info.json"), &info)?;
    assert_eq!(info.session_id, original.session_id);
    assert_eq!(info.state, SessionState::Ready);
    assert_eq!(info.session_model, SessionModel::LiveProcess);
    assert_eq!(info.recovery_level, RecoveryLevel::R1ObjectCheckpoint);
    assert_eq!(info.cells_executed, original.seq);
    assert_eq!(fs::read(&state_path)?, checkpoint_bytes);
    assert_eq!(fs::read(&checkpoint_path)?, checkpoint_bytes, "original immutable checkpoint changed");
    assert_eq!(fs::read(&witness)?, EFFECT_BYTES, "restore repeated the external effect");

    let read = recovered.execute(cell(
        "assert mark == {'label': 'persisted-42', 'values': [2, 4, 6]}\nprint('read', mark['label'], mark['values'])"))?;
    write_evidence(&root.join("read-result.json"), &read)?;
    assert_eq!(read.record.outcome, CellOutcome::Completed { exit_code: 0 },
        "restored mark is unavailable: stdout={:?} stderr={:?}", read.stdout.text, read.stderr.text);
    assert_eq!(read.record.seq, original.seq + 1, "restore must not manufacture another cell");
    assert_eq!(read.stdout.text.trim(), "read persisted-42 [2, 4, 6]");
    assert_eq!(fs::read(&witness)?, EFFECT_BYTES, "reading restored objects repeated the external effect");
    assert_eq!(fs::read(&checkpoint_path)?, checkpoint_bytes, "later cells rewrote the original checkpoint");
    let journal = fs::read(&journal_path)?;
    assert!(journal.starts_with(&original_journal), "reopen rewrote the producer's original journal bytes");
    for line in std::str::from_utf8(&journal)?.lines() {
        let record: serde_json::Value = serde_json::from_str(line)?;
        assert_ne!(record["kind"], "replay", "R1 recovery appended a source replay record");
    }
    drop(recovered);
    assert_eq!(fs::read(&witness)?, EFFECT_BYTES, "interpreter teardown repeated the external effect");
    Ok(())
}

#[test]
fn real_python_wasm_executes_cells_with_stdlib_and_state() -> Result<()> {
    let Some(dir) = artifact_dir()? else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM is not set");
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

    // Driver cells share one persistent live interpreter: the second cell
    // reads `x` from that interpreter's namespace. The recovery test above
    // separately checks R1 restoration after the producer process exits.
    let r2 = session.execute(cell("print('carry', x)"))?;
    assert_eq!(r2.stdout.text.trim(), "carry 3628800",
        "state did not persist in the live interpreter: stdout={:?} stderr={:?}",
        r2.stdout.text, r2.stderr.text);

    std::fs::remove_dir_all(&root).ok();
    Ok(())
}

/// R083 on a real artifact: install a real pure-Python wheel into a candidate
/// environment through install_dependencies, then import it in a fresh cell.
/// Gated on the wheel path via SHENKO_REAL_PYTHON_WHEEL.
#[test]
fn real_python_wasm_installs_wheel_then_imports_it() -> Result<()> {
    let Some(dir) = artifact_dir()? else {
        eprintln!("SKIP: SHENKO_REAL_PYTHON_WASM is not set");
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
