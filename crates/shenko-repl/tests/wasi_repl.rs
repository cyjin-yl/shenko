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

/// State and explicit streams persist across separate guest processes.
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

    fs::remove_dir_all(&root).ok();
    Ok(())
}

/// A15 recovery boundary: journaled Pure cells rebuild state by
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

    let (_restored, report) = WasiReplProvider::recover(&dir, 0)?;
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

    // Rebuild starts from the empty baseline, not the surviving value four.
    let state = fs::read(dir.join("home/state.bin"))?;
    assert_eq!(state, vec![2u8], "state is reconstructed from pure cells only");

    // Four cells, restore start/end, and two replay audit lines.
    let journal = fs::read_to_string(dir.join("journal.jsonl"))?;
    assert_eq!(journal.lines().count(), 8);

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn reset_boundary_prevents_pre_reset_replay() -> Result<()> {
    let root = temp_root("reset-boundary");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(create_request(module))?;
    session.execute(cell("a", CellKind::Pure))?;
    session.reset()?;
    session.execute(cell("b", CellKind::Pure))?;
    let dir = session.session_dir().to_path_buf();
    drop(session);
    let (_recovered, report) = WasiReplProvider::recover(&dir, 0)?;
    assert!(matches!(report.entries[0].recovery, CellRecovery::BeforeReset { .. }));
    assert!(matches!(report.entries[1].recovery, CellRecovery::Replayed { .. }));
    assert_eq!(fs::read(dir.join("home/state.bin"))?, vec![1]);
    Ok(())
}

#[test]
fn corrupted_effect_stream_is_not_accepted_as_receipt() -> Result<()> {
    let root = temp_root("corrupt-stream");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(create_request(module))?;
    let result = session.execute(cell("effect", CellKind::DeclaredEffect))?;
    let dir = session.session_dir().to_path_buf();
    fs::write(dir.join(result.record.stdout_path), b"altered")?;
    drop(session);
    let (_recovered, report) = WasiReplProvider::recover(&dir, 0)?;
    assert!(matches!(report.entries[0].recovery, CellRecovery::Corrupt { .. }));
    Ok(())
}

#[test]
fn duplicate_cell_is_rejected_before_replay() -> Result<()> {
    let root = temp_root("duplicate-cell");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(create_request(module))?;
    session.execute(cell("a", CellKind::Pure))?;
    let dir = session.session_dir().to_path_buf();
    drop(session);
    let path = dir.join("journal.jsonl");
    let line = fs::read_to_string(&path)?;
    fs::write(&path, format!("{line}{line}"))?;
    let error = WasiReplProvider::recover(&dir, 0).err().unwrap();
    assert!(format!("{error:#}").contains("REPL_JOURNAL_CORRUPT"));
    assert_eq!(fs::read(dir.join("home/state.bin"))?, vec![1]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn guest_symlink_cannot_redirect_reset_outside_preopen() -> Result<()> {
    use std::os::unix::fs::symlink;
    let root = temp_root("state-symlink");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut request = create_request(module);
    request.kernel.persistence = StatePersistence::GuestFile { guest_path: "/home/link/journal.jsonl".into() };
    let mut session = provider.create(request)?;
    let dir = session.session_dir().to_path_buf();
    symlink("..", dir.join("home/link"))?;
    // A confined resolver may reject the link or report the target absent;
    // neither outcome may remove the journal outside the preopen.
    let _ = session.reset();
    assert!(dir.join("journal.jsonl").is_file());
    Ok(())
}

#[test]
fn extra_preopen_cannot_contain_private_session() -> Result<()> {
    use shenko_wasm::{MountMode, MountSpec};
    let root = temp_root("mount-overlap");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut request = create_request(module);
    request.kernel.extra_mounts.push(MountSpec::new(root.canonicalize()?, "/project", MountMode::ReadWrite, true, false));
    let error = provider.create(request).err().unwrap();
    assert!(format!("{error:#}").contains("REPL_MOUNT_CONFLICT"));
    Ok(())
}

#[test]
fn interrupted_replay_orphan_does_not_block_next_attempt() -> Result<()> {
    use shenko_types::ids::EntityId;
    use std::io::Write;
    let root = temp_root("interrupted-replay");
    let module = write_module(&root, "kernel.wat", STATEFUL_GUEST);
    let provider = WasiReplProvider::new(root.join("sessions"));
    let mut session = provider.create(create_request(module))?;
    session.execute(cell("a", CellKind::Pure))?;
    let info = session.inspect()?;
    let dir = session.session_dir().to_path_buf();
    drop(session);
    let interrupted = EntityId::new();
    let orphan = EntityId::new();
    let path = dir.join("replays").join(format!("{orphan}.source"));
    fs::write(&path, b"interrupted source")?;
    let boundary = BoundaryRecord { kind: "restore_begin".into(), schema_version: JOURNAL_SCHEMA,
        session_id: info.session_id, id: interrupted, seq: 0, after_cell_seq: 1 };
    let mut journal = fs::OpenOptions::new().append(true).open(dir.join("journal.jsonl"))?;
    serde_json::to_writer(&mut journal, &boundary)?;
    journal.write_all(b"\n")?;
    journal.sync_all()?;
    let mut reopened = provider.reopen(&dir)?;
    assert_eq!(reopened.inspect()?.state, SessionState::RecoveryRequired);
    assert!(reopened.execute(cell("unsafe", CellKind::Pure)).is_err());
    let report = reopened.restore()?;
    assert!(matches!(report.entries[0].recovery, CellRecovery::Replayed { .. }));
    assert_eq!(fs::read(&path)?, b"interrupted source");
    assert_eq!(fs::read(dir.join("home/state.bin"))?, vec![1]);
    Ok(())
}



