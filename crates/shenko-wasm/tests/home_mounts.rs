//! Integration coverage for the persistent WASM home and mount contract
//! (SPEC §8.3, C05 slice D): real preview1 guests and the public branch
//! lifecycle APIs. Each guest is an actual compiled module; no host
//! path is substituted for a declared mount and nothing is mocked.
//!
//! Scenarios, each independently verifiable:
//!
//! 1. A guest creates nested directories under `/home` and writes state; a
//!    brand-new `WasmRuntime` (the same process-replacement boundary the
//!    CLI crosses) admits the same mounts and the guest reads the bytes back.
//! 2. A managed COW mount keeps its base readonly on disk, copies writes up
//!    into the branch overlay, records durable tombstones for deletions, and
//!    a sibling branch keeps seeing the base view (branch isolation).
//! 3. A declared mount whose source does not exist is an explicit
//!    construction error — never a silent substitution of another host
//!    directory — and a guest cannot escape its mount root through `..`.

use anyhow::Result;
use shenko_wasm::{
    BranchStore, MountMode, MountSpec, WasiCommand, WasmRuntime, HOME_GUEST_PATH,
    mount_key,
};
use shenko_wasm::mount::OwnedDir;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::var_os("SHENKO_WASM_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root
        .join(format!("shenko-wasm-home-{tag}-{}", shenko_types::ids::EntityId::new()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn no_stdio() -> WasiCommand<std::io::Empty, std::io::Empty, std::io::Empty> {
    WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty())
}

fn stdout_stdio() -> (WasiCommand<std::io::Empty, MemoryOutputPipe, std::io::Empty>, MemoryOutputPipe) {
    let stdout = MemoryOutputPipe::new(64 * 1024);
    (WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty()), stdout)
}

fn compile(wat: &str) -> Vec<u8> {
    wat::parse_str(wat).expect("fixture module must assemble")
}

fn exit_code(runtime: &mut WasmRuntime, module: &[u8]) -> i32 {
    match runtime
        .run_wasi_module(module, no_stdio())
        .expect("guest run")
    {
        shenko_wasm::WasiRunOutcome::Exited(code) => code,
        other => panic!("expected Exited, got {other:?}"),
    }
}

/// Guest writes `guest-state-v1\n` into `/home/nested/one/two/state.txt`,
/// creating every level of the nested directory chain.
const WRITE_NESTED_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_create_directory"
    (func $mkdir (param i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close"
    (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "nested")
  (data (i32.const 110) "nested/one")
  (data (i32.const 120) "nested/one/two")
  (data (i32.const 140) "nested/one/two/state.txt")
  (data (i32.const 200) "guest-state-v1\0a")
  ;; mkdir allowing ERRNO_EXIST(20); any other errno exits with $code.
  (func $mkdir_lenient (param $fd i32) (param $ptr i32) (param $len i32) (param $code i32)
    (local $r i32)
    (local.set $r (call $mkdir (local.get $fd) (local.get $ptr) (local.get $len)))
    (if (i32.and (i32.ne (local.get $r) (i32.const 0))
                 (i32.ne (local.get $r) (i32.const 20)))
      (then (call $exit (local.get $code)))))
  (func (export "_start") (local $fd i32) (local $r i32)
    (call $mkdir_lenient (i32.const 3) (i32.const 100) (i32.const 6)  (i32.const 91))
    (call $mkdir_lenient (i32.const 3) (i32.const 110) (i32.const 10) (i32.const 92))
    (call $mkdir_lenient (i32.const 3) (i32.const 120) (i32.const 14) (i32.const 93))
    (i32.store (i32.const 0) (i32.const 200))
    (i32.store (i32.const 4) (i32.const 15))
    (local.set $r (call $path_open
      (i32.const 3) (i32.const 0) (i32.const 140) (i32.const 24)
      (i32.const 9) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 8)))
    (if (local.get $r) (then (call $exit (i32.const 94))))
    (local.set $fd (i32.load (i32.const 8)))
    (if (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 12))
      (then (call $exit (i32.const 95))))
    (if (i32.ne (i32.load (i32.const 12)) (i32.const 15))
      (then (call $exit (i32.const 96))))
    (if (call $fd_close (local.get $fd)) (then (call $exit (i32.const 97)))))
)
"#;

/// Guest opens `/home/nested/one/two/state.txt`, reads it and echoes the
/// bytes to stdout. A nonzero `path_open` exits 81.
const READ_NESTED_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close"
    (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 140) "nested/one/two/state.txt")
  (func (export "_start") (local $fd i32) (local $r i32) (local $n i32)
    (local.set $r (call $path_open
      (i32.const 3) (i32.const 0) (i32.const 140) (i32.const 24)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 8)))
    (if (local.get $r) (then (call $exit (i32.const 81))))
    (local.set $fd (i32.load (i32.const 8)))
    (i32.store (i32.const 0) (i32.const 1024))
    (i32.store (i32.const 4) (i32.const 256))
    (if (call $fd_read (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))
      (then (call $exit (i32.const 82))))
    (local.set $n (i32.load (i32.const 16)))
    (i32.store (i32.const 4) (local.get $n))
    (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))
      (then (call $exit (i32.const 83))))
    (if (call $fd_close (local.get $fd)) (then (call $exit (i32.const 84)))))
)
"#;

/// Guest on the second preopen (`/work`, managed COW): rewrites `keep.txt`
/// (copy-up), deletes `gone.txt` (tombstone), creates `newdir/added.txt`
/// (fresh overlay path) and rewrites `sub/nested.txt` (copy-up through
/// materialized ancestors). Distinct exit codes per stage.
const COW_MUTATE_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_create_directory"
    (func $mkdir (param i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_unlink_file"
    (func $unlink (param i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close"
    (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "keep.txt")
  (data (i32.const 112) "gone.txt")
  (data (i32.const 124) "newdir")
  (data (i32.const 132) "newdir/added.txt")
  (data (i32.const 152) "sub/nested.txt")
  (data (i32.const 200) "changed\0a")
  (data (i32.const 210) "added\0a")
  (data (i32.const 220) "changed-nested\0a")
  ;; open $fd4 for write + write iov(ptr,len) + close; exits $code on error.
  (func $write_into
      (param $ptr i32) (param $len i32) (param $data i32) (param $data_len i32)
      (param $open_code i32) (param $write_code i32)
    (local $fd i32) (local $r i32)
    (i32.store (i32.const 0) (local.get $data))
    (i32.store (i32.const 4) (local.get $data_len))
    (local.set $r (call $path_open
      (i32.const 4) (i32.const 0) (local.get $ptr) (local.get $len)
      (i32.const 9) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 8)))
    (if (local.get $r) (then (call $exit (local.get $open_code))))
    (local.set $fd (i32.load (i32.const 8)))
    (if (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 12))
      (then (call $exit (local.get $write_code))))
    (if (call $fd_close (local.get $fd))
      (then (call $exit (local.get $write_code)))))
  (func (export "_start") (local $r i32)
    ;; copy-up: keep.txt exists only in the base layer until this write.
    (call $write_into
      (i32.const 100) (i32.const 8) (i32.const 200) (i32.const 8)
      (i32.const 91) (i32.const 92))
    ;; tombstone: gone.txt disappears from the branch view only.
    (local.set $r (call $unlink (i32.const 4) (i32.const 112) (i32.const 8)))
    (if (local.get $r) (then (call $exit (i32.const 93))))
    ;; fresh overlay directory and file.
    (local.set $r (call $mkdir (i32.const 4) (i32.const 124) (i32.const 6)))
    (if (local.get $r) (then (call $exit (i32.const 94))))
    (call $write_into
      (i32.const 132) (i32.const 16) (i32.const 210) (i32.const 6)
      (i32.const 95) (i32.const 96))
    ;; copy-up under a materialized ancestor directory.
    (call $write_into
      (i32.const 152) (i32.const 14) (i32.const 220) (i32.const 15)
      (i32.const 97) (i32.const 98)))
)
"#;

/// Guest reads `keep.txt` and `gone.txt` from the second preopen and echoes
/// both to stdout. Each failed open exits with the given code, so the host
/// can tell a missing file from a successful read.
const READ_KEEP_GONE_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close"
    (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 100) "keep.txt")
  (data (i32.const 112) "gone.txt")
  (func $echo
      (param $ptr i32) (param $len i32) (param $code i32)
    (local $fd i32) (local $r i32) (local $n i32)
    (local.set $r (call $path_open
      (i32.const 4) (i32.const 0) (local.get $ptr) (local.get $len)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 8)))
    (if (local.get $r) (then (call $exit (local.get $code))))
    (local.set $fd (i32.load (i32.const 8)))
    (i32.store (i32.const 0) (i32.const 1024))
    (i32.store (i32.const 4) (i32.const 256))
    (if (call $fd_read (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))
      (then (call $exit (i32.const 88))))
    (local.set $n (i32.load (i32.const 16)))
    (i32.store (i32.const 4) (local.get $n))
    (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))
      (then (call $exit (i32.const 89))))
    (if (call $fd_close (local.get $fd)) (then (call $exit (i32.const 90)))))
  (func (export "_start")
    (call $echo (i32.const 100) (i32.const 8) (i32.const 81))
    (call $echo (i32.const 112) (i32.const 8) (i32.const 82)))
)
"#;

/// Guest on fd 3 attempts a mount-root escape: `../outside.txt` must not be
/// openable. If the open succeeds the guest proves host access by writing
/// the file's first bytes to stdout and exiting 90.
const TRAVERSAL_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 140) "../outside.txt")
  (func (export "_start") (local $fd i32) (local $r i32)
    (local.set $r (call $path_open
      (i32.const 3) (i32.const 0) (i32.const 140) (i32.const 14)
      (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 8)))
    (if (i32.eq (local.get $r) (i32.const 0))
      (then
        (local.set $fd (i32.load (i32.const 8)))
        (i32.store (i32.const 0) (i32.const 1024))
        (i32.store (i32.const 4) (i32.const 64))
        (if (call $fd_read (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))
          (then (call $exit (i32.const 89))))
        (i32.store (i32.const 4) (i32.load (i32.const 16)))
        (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))
          (then (call $exit (i32.const 91))))
        (call $exit (i32.const 90)))))
)
"#;

/// Scenario 1: guest writes nested state; a newly constructed runtime over
/// the same home reads it back. The two `WasmRuntime` instances are the
/// crate-level analogue of the two CLI processes in the C05 recovery
/// evidence — everything durable must come from the home on disk.
#[test]
fn guest_writes_nested_state_and_a_reopened_runtime_reads_it_back() -> Result<()> {
    let home = temp_dir("reopen");
    let mounts = || {
        vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )]
    };
    let writer = compile(WRITE_NESTED_WAT);
    {
        let mut first = WasmRuntime::with_mounts(mounts())?;
        assert_eq!(exit_code(&mut first, &writer), 0, "writer guest");
    } // first runtime (and its engine/store) is fully dropped here

    let state = home.join("nested/one/two/state.txt");
    assert_eq!(
        fs::read(&state)?,
        b"guest-state-v1\n",
        "guest write must land in the persistent home"
    );

    // Reopen: a fresh runtime re-admits the mounts and the guest reads the
    // bytes through its own preopen. Nothing is shared with the dropped run.
    let mut second = WasmRuntime::with_mounts(mounts())?;
    let (command, stdout) = stdout_stdio();
    let outcome = second.run_wasi_module(&compile(READ_NESTED_WAT), command)?;
    assert_eq!(outcome, shenko_wasm::WasiRunOutcome::Exited(0), "reader guest");
    assert_eq!(
        stdout.contents().as_ref(),
        b"guest-state-v1\n",
        "reopened runtime must serve the same bytes the earlier run wrote"
    );

    fs::remove_dir_all(&home).ok();
    Ok(())
}

/// Scenario 2: managed COW. The base stays byte-identical, the branch
/// overlay carries copy-ups and fresh paths, deletions are durable
/// tombstones, and a sibling branch still sees the untouched base.
#[test]
fn cow_mount_keeps_base_unchanged_and_isolates_branches() -> Result<()> {
    let home = temp_dir("cow-home");
    let base = temp_dir("cow-base");
    fs::write(base.join("keep.txt"), b"base-keep\n")?;
    fs::write(base.join("gone.txt"), b"base-gone\n")?;
    fs::create_dir_all(base.join("sub"))?;
    fs::write(base.join("sub/nested.txt"), b"base-nested\n")?;
    fs::create_dir_all(base.join("untouched"))?;
    let preserved = [0x00, 0xff, 0x80, 0x0a, 0x0d, 0x42];
    fs::write(base.join("untouched/preserved.bin"), preserved)?;

    let cow_mount = |branch: &str| {
        MountSpec::cow(
            base.clone(),
            "/work",
            "main",
            branch,
            false,
            true,
        )
    };
    let home_mount = || {
        MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )
    };

    // Branch `main` mutates its effective view.
    {
        let mut rt = WasmRuntime::with_mounts(vec![home_mount(), cow_mount("main")])?;
        assert_eq!(exit_code(&mut rt, &compile(COW_MUTATE_WAT)), 0, "COW writer guest");
    }

    // The base is byte-identical: COW is never relabelled RW.
    assert_eq!(fs::read(base.join("keep.txt"))?, b"base-keep\n");
    assert_eq!(fs::read(base.join("gone.txt"))?, b"base-gone\n");
    assert_eq!(fs::read(base.join("sub/nested.txt"))?, b"base-nested\n");
    assert_eq!(fs::read(base.join("untouched/preserved.bin"))?, preserved);
    assert!(
        !base.join("newdir").exists(),
        "guest must not create paths in the readonly base"
    );

    // The branch store records exactly this mount's overlay + tombstones.
    let key = mount_key("/work");
    let owned_home = OwnedDir::admit_absolute(&home)?;
    let branch = BranchStore::open(&owned_home, "main", "main")?;
    let overlay = home.join(branch.rel_overlay(&key));
    assert_eq!(fs::read(overlay.join("keep.txt"))?, b"changed\n");
    assert_eq!(fs::read(overlay.join("newdir/added.txt"))?, b"added\n");
    assert_eq!(fs::read(overlay.join("sub/nested.txt"))?, b"changed-nested\n");
    let tombstones = fs::read(home.join(branch.rel_tombstones(&key)))?;
    assert!(
        String::from_utf8(tombstones)?.contains("gone.txt"),
        "deletion must leave a durable tombstone"
    );
    // The effective-view diff names exactly the guest's changes.
    let diff = branch.diff_abs(&home, &key, &base)?;
    assert_eq!(
        diff.added,
        BTreeSet::from(["newdir".to_owned(), "newdir/added.txt".to_owned()])
    );
    assert_eq!(
        diff.modified,
        BTreeSet::from(["keep.txt".to_owned(), "sub/nested.txt".to_owned()])
    );
    assert_eq!(diff.removed, BTreeSet::from(["gone.txt".to_owned()]));

    // A second runtime over branch `main` still sees its own view: the
    // branch state survives runtime replacement, not just runtime lifetime.
    {
        let mut rt = WasmRuntime::with_mounts(vec![home_mount(), cow_mount("main")])?;
        let (command, stdout) = stdout_stdio();
        // reuse the read guest: keep.txt -> "changed\n", gone.txt open fails
        // with exit 82 proving the tombstone persists across runtime reopen.
        let outcome = rt.run_wasi_module(&compile(READ_KEEP_GONE_WAT), command)?;
        assert_eq!(
            outcome,
            shenko_wasm::WasiRunOutcome::Exited(82),
            "gone.txt must be removed in the main branch view"
        );
        assert_eq!(stdout.contents().as_ref(), b"changed\n");
    }

    // A sibling branch `other` sees the untouched base view instead.
    {
        let mut rt = WasmRuntime::with_mounts(vec![home_mount(), cow_mount("other")])?;
        let (command, stdout) = stdout_stdio();
        let outcome = rt.run_wasi_module(&compile(READ_KEEP_GONE_WAT), command)?;
        assert_eq!(outcome, shenko_wasm::WasiRunOutcome::Exited(0));
        assert_eq!(
            stdout.contents().as_ref(),
            b"base-keep\nbase-gone\n",
            "sibling branch must see the base, not main's overlay"
        );
    }

    let sibling = BranchStore::open(&owned_home, "main", "other")?;
    let sibling_diff = sibling.diff_abs(&home, &key, &base)?;
    assert_eq!(sibling_diff.added, BTreeSet::new());
    assert_eq!(sibling_diff.modified, BTreeSet::new());
    assert_eq!(sibling_diff.removed, BTreeSet::new());
    assert_eq!(fs::read(base.join("untouched/preserved.bin"))?, preserved);

    fs::remove_dir_all(&home).ok();
    fs::remove_dir_all(&base).ok();
    Ok(())
}

/// A tombstone open failure must reject mount admission before another guest
/// can run. Retain the original state and the synthetic malicious input.
#[cfg(unix)]
#[test]
fn cow_tombstone_open_failure_rejects_mount_readmission() -> Result<()> {
    use std::os::unix::fs::symlink;

    let home = temp_dir("cow-tombstone-fault-home");
    let base = temp_dir("cow-tombstone-fault-base");
    fs::write(base.join("keep.txt"), b"base-keep\n")?;
    fs::write(base.join("gone.txt"), b"base-gone\n")?;
    fs::create_dir_all(base.join("sub"))?;
    fs::write(base.join("sub/nested.txt"), b"base-nested\n")?;
    let mounts = || {
        vec![
            MountSpec::new(
                home.clone(),
                HOME_GUEST_PATH,
                MountMode::ReadWrite,
                true,
                false,
            ),
            MountSpec::cow(base.clone(), "/work", "main", "main", false, true),
        ]
    };
    {
        let mut rt = WasmRuntime::with_mounts(mounts())?;
        assert_eq!(exit_code(&mut rt, &compile(COW_MUTATE_WAT)), 0);
    }

    let owned_home = OwnedDir::admit_absolute(&home)?;
    let branch = BranchStore::open(&owned_home, "main", "main")?;
    let key = mount_key("/work");
    let tombstones = home.join(branch.rel_tombstones(&key));
    let saved_tombstones = tombstones.with_extension("saved.json");
    let original_bytes = fs::read(&tombstones)?;
    let recorded: serde_json::Value = serde_json::from_slice(&original_bytes)?;
    assert_eq!(recorded["removed"], serde_json::json!(["gone.txt"]));
    fs::rename(&tombstones, &saved_tombstones)?;
    // Relative same-basename target deliberately loops at this owned fixture.
    let basename = tombstones.file_name().expect("tombstone filename");
    symlink(basename, &tombstones)?;

    let mut runtime = WasmRuntime::with_mounts(mounts())?;
    let (command, stdout) = stdout_stdio();
    let execution = runtime.run_wasi_module(&compile(READ_KEEP_GONE_WAT), command);
    assert!(execution.is_err());
    assert_eq!(stdout.contents().as_ref(), b"");
    assert_eq!(fs::read(&saved_tombstones)?, original_bytes);
    assert_eq!(fs::read_link(&tombstones)?, PathBuf::from(basename));
    assert_eq!(fs::read(base.join("keep.txt"))?, b"base-keep\n");
    assert_eq!(fs::read(base.join("gone.txt"))?, b"base-gone\n");
    assert_eq!(fs::read(base.join("sub/nested.txt"))?, b"base-nested\n");
    assert!(!base.join("newdir").exists());
    Ok(())
}

/// Copy-up with identical binary bytes is unchanged; a late byte change is
/// visible. Unreadable data must not compare equal and publish a new head.
#[cfg(unix)]
#[test]
fn cow_unreadable_contents_refuse_diff_and_commit() -> Result<()> {
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    let home = temp_dir("cow-content-home");
    let base = temp_dir("cow-content-base");
    let original = (0..100_003).map(|value| value as u8).collect::<Vec<_>>();
    fs::write(base.join("same.bin"), &original)?;
    let owned_home = OwnedDir::admit_absolute(&home)?;
    let branch = BranchStore::open(&owned_home, "main", "main")?;
    let key = mount_key("/work");
    let mount = branch.mount(&owned_home, &key)?;
    mount.overlay_dir().write("same.bin", &original)?;
    assert!(branch.diff_abs(&home, &key, &base)?.is_empty());

    let mut changed = original.clone();
    *changed.last_mut().unwrap() ^= 0xff;
    mount.overlay_dir().write("same.bin", &changed)?;
    let diff = branch.diff_abs(&home, &key, &base)?;
    assert_eq!(diff.modified, BTreeSet::from(["same.bin".to_owned()]));
    assert!(diff.added.is_empty());
    assert!(diff.removed.is_empty());
    let bases = BTreeMap::from([(key.clone(), base.clone())]);
    let cas = shenko_store::cas::ContentStore::new(temp_dir("cow-content-cas"))?;
    let outcome = branch.commit_abs(&home, &cas, &bases)?;
    assert!(outcome.published);
    let commits_path = home.join(branch.rel_commits_file());
    let commits_before = fs::read(&commits_path)?;

    let overlay = home.join(branch.rel_overlay(&key));
    let saved_base = home.join("saved-base.bin");
    let saved_overlay = home.join("saved-overlay.bin");
    fs::rename(base.join("same.bin"), &saved_base)?;
    fs::rename(overlay.join("same.bin"), &saved_overlay)?;
    symlink("same.bin", base.join("same.bin"))?;
    symlink("same.bin", overlay.join("same.bin"))?;
    assert!(branch.diff_abs(&home, &key, &base).is_err());
    assert!(branch.commit_abs(&home, &cas, &bases).is_err());
    assert_eq!(branch.head_abs(&home)?, Some(outcome.id.clone()));
    assert_eq!(fs::read(&commits_path)?, commits_before);
    assert_eq!(fs::read(&saved_base)?, original);
    assert_eq!(fs::read(&saved_overlay)?, changed);
    Ok(())
}

/// A relative overlay symlink replaces the base directory itself; an absent
/// target child must not reappear from the old base subtree.
#[cfg(unix)]
#[test]
fn cow_overlay_symlink_masks_base_descendants() -> Result<()> {
    use std::os::unix::fs::symlink;

    let home = temp_dir("cow-link-home");
    let base = temp_dir("cow-link-base");
    fs::create_dir(base.join("linked"))?;
    fs::write(base.join("linked/old.bin"), [0x00, 0xff, 0x42])?;
    let owned_home = OwnedDir::admit_absolute(&home)?;
    let branch = BranchStore::open(&owned_home, "main", "main")?;
    let key = mount_key("/work");
    let mount = branch.mount(&owned_home, &key)?;
    mount.overlay_dir().create_dir("target")?;
    symlink("target", home.join(branch.rel_overlay(&key)).join("linked"))?;
    let mut runtime = WasmRuntime::with_mounts(vec![
        MountSpec::new(home.clone(), HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
        MountSpec::cow(base.clone(), "/work", "main", "main", false, true),
    ])?;
    let guest = compile(r#"(module
        (import "wasi_snapshot_preview1" "path_open"
            (func $open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
        (memory (export "memory") 1)
        (data (i32.const 100) "linked/old.bin")
        (func (export "_start")
            (if (i32.eqz (call $open (i32.const 4) (i32.const 0)
                (i32.const 100) (i32.const 14) (i32.const 0)
                (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 0)))
                (then (call $exit (i32.const 88))))))"#);
    assert_eq!(exit_code(&mut runtime, &guest), 0);
    let diff = branch.diff_abs(&home, &key, &base)?;
    assert_eq!(diff.added, BTreeSet::from(["target".to_owned()]));
    assert_eq!(diff.modified, BTreeSet::from(["linked".to_owned()]));
    assert_eq!(diff.removed, BTreeSet::from(["linked/old.bin".to_owned()]));
    assert_eq!(fs::read(base.join("linked/old.bin"))?, [0x00, 0xff, 0x42]);
    Ok(())
}

/// Replacing a base directory hides its descendants; replacing a base file
/// with a directory adds the directory's children to the effective view.
#[test]
fn cow_diff_accounts_for_file_directory_replacements() -> Result<()> {
    let home = temp_dir("cow-type-home");
    let base = temp_dir("cow-type-base");
    fs::create_dir(base.join("directory"))?;
    fs::write(base.join("directory/old.bin"), [0x00, 0xff, 0x42])?;
    fs::write(base.join("file"), b"base-file\n")?;
    let owned_home = OwnedDir::admit_absolute(&home)?;
    let branch = BranchStore::open(&owned_home, "main", "main")?;
    let key = mount_key("/work");
    let mount = branch.mount(&owned_home, &key)?;
    mount.overlay_dir().write("directory", b"replacement-file\n")?;
    mount.overlay_dir().create_dir("file")?;
    mount.overlay_dir().write("file/new.bin", [0x80, 0x00, 0x0a])?;

    let diff = branch.diff_abs(&home, &key, &base)?;
    assert_eq!(diff.added, BTreeSet::from(["file/new.bin".to_owned()]));
    assert_eq!(
        diff.modified,
        BTreeSet::from(["directory".to_owned(), "file".to_owned()])
    );
    assert_eq!(diff.removed, BTreeSet::from(["directory/old.bin".to_owned()]));
    assert_eq!(fs::read(base.join("directory/old.bin"))?, [0x00, 0xff, 0x42]);
    assert_eq!(fs::read(base.join("file"))?, b"base-file\n");
    Ok(())
}

/// Lifecycle capture must refuse a vanished required root without publishing
/// a different head or appending a partial commit. Keep all fixture bytes.
#[test]
fn cow_missing_roots_refuse_commit_without_advancing_head() -> Result<()> {
    use std::collections::BTreeMap;

    let home = temp_dir("cow-root-home");
    let base = temp_dir("cow-root-base");
    fs::write(base.join("keep.txt"), b"base-keep\n")?;
    fs::write(base.join("gone.txt"), b"base-gone\n")?;
    fs::create_dir(base.join("sub"))?;
    fs::write(base.join("sub/nested.txt"), b"base-nested\n")?;
    {
        let mut runtime = WasmRuntime::with_mounts(vec![
            MountSpec::new(home.clone(), HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
            MountSpec::cow(base.clone(), "/work", "main", "main", false, true),
        ])?;
        assert_eq!(exit_code(&mut runtime, &compile(COW_MUTATE_WAT)), 0);
    }

    let owned_home = OwnedDir::admit_absolute(&home)?;
    let branch = BranchStore::open(&owned_home, "main", "main")?;
    let key = mount_key("/work");
    let bases = BTreeMap::from([(key.clone(), base.clone())]);
    let cas = shenko_store::cas::ContentStore::new(temp_dir("cow-root-cas"))?;
    let outcome = branch.commit_abs(&home, &cas, &bases)?;
    assert!(outcome.published);
    assert_eq!(branch.head_abs(&home)?, Some(outcome.id.clone()));
    let commits_path = home.join(branch.rel_commits_file());
    let commits_before = fs::read(&commits_path)?;

    let saved_base = base.with_extension("saved");
    fs::rename(&base, &saved_base)?;
    assert!(branch.commit_abs(&home, &cas, &bases).is_err());
    assert_eq!(branch.head_abs(&home)?, Some(outcome.id.clone()));
    assert_eq!(fs::read(&commits_path)?, commits_before);
    assert_eq!(fs::read(saved_base.join("keep.txt"))?, b"base-keep\n");
    fs::rename(&saved_base, &base)?;

    let overlay = home.join(branch.rel_overlay(&key));
    let saved_overlay = overlay.with_extension("saved");
    fs::rename(&overlay, &saved_overlay)?;
    assert!(branch.commit_abs(&home, &cas, &bases).is_err());
    assert_eq!(branch.head_abs(&home)?, Some(outcome.id.clone()));
    assert_eq!(fs::read(&commits_path)?, commits_before);
    assert_eq!(fs::read(saved_overlay.join("keep.txt"))?, b"changed\n");
    assert_eq!(fs::read(base.join("gone.txt"))?, b"base-gone\n");
    Ok(())
}

/// Scenario 3: a declared mount whose source does not exist fails admission
/// explicitly — the runtime is never constructed with some other directory
/// silently substituted. Separately, `..` outside the mount root is rejected
/// and can never reach host files.
#[test]
fn absent_declared_mount_fails_explicitly_and_guest_stays_confined() -> Result<()> {
    let home = temp_dir("absent-home");
    let missing = home.join("deleted-declared-mount");
    // A mount source that was declared but whose directory is now absent is
    // not a valid mount; it is an explicit admission failure.
    let result = WasmRuntime::with_mounts(vec![
        MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        ),
        MountSpec::new(
            missing.clone(),
            "/absent",
            MountMode::ReadWrite,
            false,
            false,
        ),
    ]);
    let error = result.map(|_| ()).expect_err("absent mount must fail");
    let text = format!("{error:#}");
    assert!(
        text.contains("WASM_PROFILE_UNAVAILABLE"),
        "absent mount must be an explicit typed failure, got: {text}"
    );

    // A regular file is not a directory either: same explicit failure.
    let file_source = home.join("file-source");
    fs::write(&file_source, b"not-a-directory")?;
    let result = WasmRuntime::with_mounts(vec![
        MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        ),
        MountSpec::new(
            file_source,
            "/file",
            MountMode::ReadOnly,
            false,
            false,
        ),
    ]);
    assert!(result.is_err(), "file-as-mount must fail admission");

    // Confinement: an actual guest inside a valid runtime cannot escape its
    // mount root. The home sits inside a dedicated parent directory that
    // holds a host-only file; `../outside.txt` from the preopen names it.
    // A successful open would exit 90 after copying host bytes to stdout —
    // the run must instead exit 0 with empty stdout.
    let parent = temp_dir("confine-parent");
    let confined_home = parent.join("inner-home");
    fs::create_dir_all(&confined_home)?;
    let outside = parent.join("outside.txt");
    fs::write(&outside, b"host-secret")?;
    let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
        confined_home.clone(),
        HOME_GUEST_PATH,
        MountMode::ReadWrite,
        true,
        false,
    )])?;
    let (command, stdout) = stdout_stdio();
    let outcome = rt.run_wasi_module(&compile(TRAVERSAL_WAT), command)?;
    assert_eq!(outcome, shenko_wasm::WasiRunOutcome::Exited(0), "traversal must be rejected");
    assert!(stdout.contents().is_empty(), "no host bytes may reach the guest");

    fs::remove_dir_all(&parent).ok();
    fs::remove_dir_all(&home).ok();
    Ok(())
}
