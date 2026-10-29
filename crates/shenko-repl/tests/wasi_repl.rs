//! Real-execution tests for the WASI REPL backend. Every guest below is a
//! real WASI preview1 module run through `WasmRuntime`; nothing is mocked.
//! The test kernel emulates a minimal cell protocol only (stdin = source,
//! state file under /home) — it is explicitly a `JsWasm` "test-kernel",
//! never claimed to be an interpreter delivery.

use anyhow::Result;
use shenko_repl::*;
use std::fs;
use std::path::{Path, PathBuf};

/// Guest that reads one byte from stdin, maintains a 1-byte counter in
/// `/home/state.bin` (fd 3 = the /home preopen), echoes "S:<byte>" and the
/// new counter to stdout, writes nothing to stderr, exits 0.
const STATEFUL_GUEST: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close" (func $close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_seek"
    (func $seek (param i32 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 600) "state.bin")
  (data (i32.const 620) "S:")
  ;; $fail: exit with the given stage code.
  (func $fail (param i32) (call $exit (local.get 0)))
  ;; $read_byte from fd into 100; on EOF leaves 0. returns nread.
  (func $bread (param $fd i32) (result i32)
    (i32.store (i32.const 40) (i32.const 100))
    (i32.store (i32.const 44) (i32.const 1))
    (if (call $read (local.get $fd) (i32.const 40) (i32.const 1) (i32.const 48))
      (then (call $fail (i32.const 60))))
    (i32.load (i32.const 48)))
  (func $bwrite (param $fd i32) (param $ptr i32) (param $len i32)
    (i32.store (i32.const 40) (local.get $ptr))
    (i32.store (i32.const 44) (local.get $len))
    (if (call $write (local.get $fd) (i32.const 40) (i32.const 1) (i32.const 48))
      (then (call $fail (i32.const 61)))))
  (func (export "_start")
    (local $statefd i32) (local $count i32)
    ;; Read one source byte from stdin (fd 0).
    (drop (call $bread (i32.const 0)))
    (local.set $count (i32.load8_u (i32.const 100)))
    ;; Open /home/state.bin read-only: fd=3, path=600 len=9, oflags=0,
    ;; rights=FD_READ|FD_SEEK|FD_WRITE (2|4|64=70). Missing → errno → create.
    (if (call $open (i32.const 3) (i32.const 0) (i32.const 600) (i32.const 9)
                    (i32.const 0) (i64.const 70) (i64.const 0) (i32.const 0) (i32.const 56))
      (then
        ;; create+truncate: oflags=CREAT|TRUNC=9, count byte 0
        (if (call $open (i32.const 3) (i32.const 0) (i32.const 600) (i32.const 9)
                        (i32.const 9) (i64.const 70) (i64.const 0) (i32.const 0) (i32.const 56))
          (then (call $fail (i32.const 62))))
        (i32.store8 (i32.const 100) (i32.const 0))
        (call $bwrite (i32.load (i32.const 56)) (i32.const 100) (i32.const 1))))
    (local.set $statefd (i32.load (i32.const 56)))
    ;; Read existing counter; empty file reads 0.
    (if (call $seek (local.get $statefd) (i64.const 0) (i32.const 0) (i32.const 64))
      (then (call $fail (i32.const 63))))
    (drop (call $bread (local.get $statefd)))
    ;; counter += 1, persist back.
    (i32.store8 (i32.const 101) (i32.add (i32.load8_u (i32.const 100)) (i32.const 1)))
    (if (call $seek (local.get $statefd) (i64.const 0) (i32.const 0) (i32.const 64))
      (then (call $fail (i32.const 64))))
    (call $bwrite (local.get $statefd) (i32.const 101) (i32.const 1))
    (drop (call $close (local.get $statefd)))
    ;; stdout: "S:" + source byte + counter byte.
    (i32.store8 (i32.const 102) (local.get $count))
    (i32.store8 (i32.const 103) (i32.load8_u (i32.const 101)))
    (call $bwrite (i32.const 1) (i32.const 620) (i32.const 2))
    (call $bwrite (i32.const 1) (i32.const 102) (i32.const 2))))
"#;

/// Guest that writes to both streams then exits 7 — a nonzero interpreter
/// exit that is still a completed, journaled cell.
const EXIT7_GUEST: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "out-part")
  (data (i32.const 120) "err-part")
  (func (export "_start")
    (i32.store (i32.const 40) (i32.const 100))
    (i32.store (i32.const 44) (i32.const 8))
    (drop (call $write (i32.const 1) (i32.const 40) (i32.const 1) (i32.const 48)))
    (i32.store (i32.const 40) (i32.const 120))
    (i32.store (i32.const 44) (i32.const 8))
    (drop (call $write (i32.const 2) (i32.const 40) (i32.const 1) (i32.const 48)))
    (call $exit (i32.const 7))))
"#;

/// Guest that writes a stdout prefix then traps — a real failed cell whose
/// pre-trap output must stay durable.
const TRAP_GUEST: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "before-trap")
  (func (export "_start")
    (i32.store (i32.const 40) (i32.const 100))
    (i32.store (i32.const 44) (i32.const 11))
    (drop (call $write (i32.const 1) (i32.const 40) (i32.const 1) (i32.const 48)))
    unreachable))
"#;

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-repl-{tag}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_module(root: &Path, name: &str, wat: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, wat).unwrap();
    path
}

fn kernel(module: PathBuf) -> KernelProfile {
    KernelProfile {
        kernel_id: "test-kernel".into(),
        runtime: ReplRuntime::JsWasm,
        runtime_label: "test-cell-protocol wasi-preview1".into(),
        backend: BackendKind::WasiPreview1,
        module: ModuleSource::ModulePath(module),
        argv: vec!["test-kernel".into(), "--cell".into()],
        env: vec![("REPL_KIND".into(), "test".into())],
        workdir_guest: "/home".into(),
        persistence: StatePersistence::GuestFile {
            guest_path: "/home/state.bin".into(),
        },
        extra_mounts: vec![],
        dependency_lock: vec![],
    }
}

fn create_request(module: PathBuf) -> CreateSession {
    CreateSession {
        kernel: kernel(module),
        bind: TaskBinding {
            task: None,
            worker: None,
            scope: Some("test".into()),
        },
        limits: ResourceLimits::default(),
        preview_bytes: 0,
    }
}

fn cell(source: &str, kind: CellKind) -> CellRequest {
    CellRequest {
        source: source.as_bytes().to_vec(),
        cell_kind: kind,
        labels: Default::default(),
    }
}

/// Scenario 1 (end-to-end happy path + restart reopen + R1-via-file state):
/// create a session bound to a real WASI module, execute two cells, prove
/// stdin delivery + guest-file state persistence across cell processes,
/// R0 journal durability, checkpoint digests, reset semantics, then drop the
/// session and reopen it from disk proving the journal survives a "restart".
#[test]
fn session_executes_cells_with_durable_r0_and_guest_file_state() -> Result<()> {
    let root = temp_root("execute");
    let sessions = root.join("sessions");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(sessions.clone());
    let mut session = provider.create(create_request(module))?;

    // Two cells: each echoes "S:" + source byte + state counter. Counter must
    // persist across the two distinct guest processes via /home/state.bin.
    let r1 = session.execute(cell("A", CellKind::Pure))?;
    assert_eq!(r1.record.seq, 1);
    assert_eq!(r1.record.outcome, CellOutcome::Completed { exit_code: 0 });
    assert_eq!(r1.stdout.text, "S:A\u{1}");
    assert!(!r1.stdout.truncated);
    let r2 = session.execute(cell("B", CellKind::Pure))?;
    assert_eq!(r2.stdout.text, "S:B\u{2}", "guest state file must persist across cell processes");

    // R0 durability: journal + cell files are host-side durable facts.
    let dir = session.session_dir().to_path_buf();
    assert!(dir.join("journal.jsonl").exists());
    assert!(dir.join(&r1.record.source_path).exists());
    assert!(dir.join(&r1.record.stdout_path).exists());
    let journal_text = fs::read_to_string(dir.join("journal.jsonl"))?;
    assert_eq!(journal_text.lines().count(), 2, "two durable cell records");

    // inspect reports honest capability: R0 only, no live object persistence.
    let info = session.inspect()?;
    assert_eq!(info.recovery_level, RecoveryLevel::R0WorkRecord);
    assert!(!info.interpreter_state_persists);
    assert_eq!(info.cells_executed, 2);
    assert!(!info.limits.enforced);

    // checkpoint cites the journal and the kernel's declared state file.
    let checkpoint = session.checkpoint()?;
    assert_eq!(checkpoint.cells, 2);
    assert_eq!(checkpoint.state_files.len(), 1, "state.bin digest recorded");

    // cancel is honest: nothing running, no claim of interruption.
    assert_eq!(session.cancel()?, CancelOutcome::NothingRunning);

    // reset removes the kernel-declared state file; journal stays.
    let state_file = dir.join("home/state.bin");
    assert!(state_file.exists());
    session.reset()?;
    assert!(!state_file.exists(), "reset cleared declared interpreter state");
    let state_dir = dir.clone();

    // "Restart": drop the session, reopen from the directory. The journal is
    // the source of truth; a new execute appends seq 3 — never rewinds.
    drop(session);
    let mut reopened = provider.reopen(&state_dir)?;
    assert_eq!(reopened.inspect()?.cells_executed, 2);
    let r3 = reopened.execute(cell("C", CellKind::Pure))?;
    assert_eq!(r3.record.seq, 3);
    // state.bin was reset: counter restarts at 1.
    assert_eq!(r3.stdout.text, "S:C\u{1}");

    // fork copies guest-visible state into a fresh session with its own
    // empty journal.
    let forked = reopened.fork(&sessions)?;
    assert_eq!(forked.inspect()?.cells_executed, 0);
    assert!(forked.session_dir().join("journal.jsonl").exists());
    fs::remove_dir_all(&root).ok();
    Ok(())
}

/// Scenario 2 (honest failure semantics): nonzero exit is a journaled cell
/// with streams; a trap is a journaled Failed record whose pre-trap stdout
/// remains durable; a swapped module fails binding before any new cell.
#[test]
fn failures_are_journaled_and_module_rebinding_is_rejected() -> Result<()> {
    let root = temp_root("failures");
    let module = write_module(&root, "kernel.wat", EXIT7_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(create_request(module.clone()))?;

    let r1 = session.execute(cell("boom", CellKind::Pure))?;
    assert_eq!(r1.record.outcome, CellOutcome::Exited { exit_code: 7 });
    assert_eq!(r1.stdout.text, "out-part");
    assert_eq!(r1.stderr.text, "err-part");

    // Swap the module for the trapping guest: binding must reject the new
    // bytes before the cell runs — no fake success on a different module.
    fs::write(&module, TRAP_GUEST)?;
    let err = session.execute(cell("x", CellKind::Pure)).unwrap_err();
    assert!(
        err.to_string().contains("REPL_KERNEL_UNAVAILABLE"),
        "swapped module rejected: {err}"
    );

    // Recreate against the trap module directly to prove pre-trap output is
    // journaled as durable stream bytes on a Failed cell.
    let trap_module = write_module(&root, "trap.wat", TRAP_GUEST);
    let mut trapped = provider.create(create_request(trap_module))?;
    let r2 = trapped.execute(cell("t", CellKind::Pure))?;
    match &r2.record.outcome {
        CellOutcome::Failed { error } => {
            assert!(error.contains("run WASI `_start`"), "{error}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(r2.stdout.text, "before-trap");
    assert!(r2.record.stdout_bytes > 0);
    fs::remove_dir_all(&root).ok();
    Ok(())
}

/// Scenario 3 (A15 recovery boundary): journaled Pure cells rebuild state by
/// real replay; a DeclaredEffect cell is a receipt that is never re-executed;
/// an Unknown cell is reported for reconciliation. Proof: the state counter
/// after recovery equals the number of Pure cells only.
#[test]
fn recover_replays_only_pure_cells_and_keeps_effect_receipts() -> Result<()> {
    let root = temp_root("recover");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(create_request(module))?;
    session.execute(cell("a", CellKind::Pure))?;
    session.execute(cell("b", CellKind::Pure))?;
    session.execute(cell("pay", CellKind::DeclaredEffect))?;
    session.execute(cell("mystery", CellKind::Unknown))?;
    let dir = session.session_dir().to_path_buf();
    drop(session);

    let (mut restored, report) = WasiReplProvider::recover(&dir, 0)?;
    assert_eq!(report.entries.len(), 4);
    let replays: Vec<_> = report
        .entries
        .iter()
        .filter(|e| matches!(e.recovery, CellRecovery::Replayed { .. }))
        .collect();
    assert_eq!(replays.len(), 2, "only the two Pure cells replay");
    assert!(matches!(
        report.entries[2].recovery,
        CellRecovery::ReceiptOnly
    ));
    assert!(matches!(
        report.entries[3].recovery,
        CellRecovery::NeedsReconciliation
    ));

    // Guest-visible proof of what reran: 4 original cells incremented the
    // counter to 4; the 2 pure replays increment the persisted file to 6.
    // The declared-effect and unknown cells are never re-executed by a later
    // live cell either — replay is over, seq 5 writes next.
    let state = fs::read(dir.join("home/state.bin"))?;
    assert_eq!(state, vec![6u8], "state = 4 original cells + 2 pure replays");

    // Recovery journal: 4 cells + 2 replay audit lines, in order.
    let journal = fs::read_to_string(dir.join("journal.jsonl"))?;
    assert_eq!(journal.lines().count(), 6);

    // The recovered session keeps executing with an honest monotonic seq.
    let next = restored.execute(cell("z", CellKind::Pure))?;
    assert_eq!(next.record.seq, 5);
    fs::remove_dir_all(&root).ok();
    Ok(())
}

/// Scenario 4 (capability honesty): the named targets distinguish runtimes —
/// Node and Bun are named-but-unavailable, Python-WASI and JS-in-WASM are
/// bindable as themselves only, Pyodide is a different ABI route.
#[test]
fn named_kernel_targets_report_real_availability() {
    let targets = named_targets();
    let find = |name: &str| targets.iter().find(|t| t.name == name).unwrap();
    assert!(matches!(
        find("python-wasi").availability,
        KernelAvailability::Bindable { .. }
    ));
    assert!(matches!(
        find("quickjs-wasm").availability,
        KernelAvailability::Bindable { .. }
    ));
    assert!(matches!(
        find("node-wasm").availability,
        KernelAvailability::Unavailable { .. }
    ));
    assert!(matches!(
        find("bun-wasm").availability,
        KernelAvailability::Unavailable { .. }
    ));
    assert!(matches!(
        find("pyodide").availability,
        KernelAvailability::Unavailable { .. }
    ));
    // JS-in-WASM must never double as Node: different runtime identity.
    assert_ne!(find("quickjs-wasm").runtime, find("node-wasm").runtime);
}
