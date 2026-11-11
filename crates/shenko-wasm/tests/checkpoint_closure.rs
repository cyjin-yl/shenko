//! Real `/home` + `/memory` byte closure (docs/specs/wasm-home.md#spec-8-3, docs/specs/durable-state.md#spec-10-1–docs/specs/memory.md#spec-10-5, C05).
//!
//! Every case is a real host tree, a real on-disk content-addressed store and
//! the real public API; no file content is faked and no closure is simulated.
//! The round trip is proven twice: by the host bytes after a restore, and by a
//! real preview1 guest reading the restored file through its own preopen.
//!
//! Scenarios, each independently verifiable:
//!
//! 1. Capture seals every file into CAS; a restore returns file bytes, modes,
//!    directories and symbolic links, removes entries the closure never had,
//!    and retains the state it replaced as a separate published closure.
//! 2. A closure with a missing object is refused before anything is mutated.
//! 3. A special file is refused instead of being silently dropped, and an
//!    absent mount is recorded as absent — never as an empty directory.
//! 4. Head/journal/ownership records are strict: corruption and a foreign
//!    installation are explicit failures, and an unchanged view does not
//!    republish.

use anyhow::{Result, ensure};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use shenko_wasm::checkpoint::{CheckpointEntryKind, HomeCheckpointStore};
use shenko_wasm::{
    HOME_GUEST_PATH, MountMode, MountSpec, WasiCommand, WasmRuntime,
};
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

const MEMORY_GUEST_PATH: &str = "/memory";

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::var_os("SHENKO_WASM_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!(
        "shenko-wasm-checkpoint-{tag}-{}",
        EntityId::new()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn mounts(root: &Path) -> Vec<(String, PathBuf)> {
    vec![
        (HOME_GUEST_PATH.to_string(), root.join("home")),
        (MEMORY_GUEST_PATH.to_string(), root.join("memory")),
    ]
}

fn store_for<'a>(
    cas: &'a ContentStore,
    root: &'a Path,
    installation: EntityId,
) -> HomeCheckpointStore<'a> {
    HomeCheckpointStore::new(cas, root, installation, mounts(root)).expect("checkpoint store")
}

/// Reads `/home/nested/one/state.txt` through the guest preopen and copies it
/// to stdout: the restore is only real if the guest view sees the bytes.
const READ_RESTORED_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 200) "nested/one/state.txt")
  (func (export "_start")
    (i32.store (i32.const 500) (i32.const 600))
    (i32.store (i32.const 504) (i32.const 64))
    (if (i32.ne
      (call $path_open
        (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 20)
        (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 400))
      (i32.const 0))
      (then (call $proc_exit (i32.const 70))))
    (if (i32.ne
      (call $fd_read (i32.load (i32.const 400)) (i32.const 500) (i32.const 1) (i32.const 520))
      (i32.const 0))
      (then (call $proc_exit (i32.const 71))))
    (i32.store (i32.const 504) (i32.load (i32.const 520)))
    (if (i32.ne
      (call $fd_write (i32.const 1) (i32.const 500) (i32.const 1) (i32.const 524))
      (i32.const 0))
      (then (call $proc_exit (i32.const 72))))
  )
)
"#;

fn restored_bytes_are_visible_to_a_real_guest(root: &Path) -> Result<()> {
    let mut runtime = WasmRuntime::with_mounts(vec![
        MountSpec::new(
            root.join("home"),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        ),
        MountSpec::new(
            root.join("memory"),
            MEMORY_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            true,
        ),
    ])?;
    let stdout = MemoryOutputPipe::new(64 * 1024);
    let outcome = runtime.run_wasi_module(
        READ_RESTORED_WAT.as_bytes(),
        WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty()),
    )?;
    anyhow::ensure!(
        outcome == shenko_wasm::WasiRunOutcome::Exited(0),
        "reader guest outcome {outcome:?}"
    );
    anyhow::ensure!(
        stdout.contents().as_ref() == b"guest-state-v1\n",
        "the guest preopen did not serve the restored bytes: {:?}",
        stdout.contents()
    );
    Ok(())
}

#[test]
fn capture_restore_round_trips_bytes_modes_links_and_removals() -> Result<()> {
    let root = temp_dir("roundtrip");
    let home = root.join("home");
    let memory = root.join("memory");
    fs::create_dir_all(home.join("nested/one"))?;
    fs::create_dir_all(&memory)?;
    fs::write(home.join("nested/one/state.txt"), b"guest-state-v1\n")?;
    fs::write(home.join("script.sh"), b"#!/bin/sh\ntrue\n")?;
    fs::write(memory.join("remembered.txt"), b"remembered\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(home.join("script.sh"), fs::Permissions::from_mode(0o755))?;
        std::os::unix::fs::symlink("nested/one/state.txt", home.join("state-link"))?;
    }

    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let installation = EntityId::new();
    let store = store_for(&cas, &root, installation);
    let baseline = store.capture("test baseline")?;
    ensure!(baseline.published, "first capture must publish");
    ensure!(
        store.head()? == Some(baseline.id.clone()),
        "published head does not name the new closure"
    );
    let verified = store.verify(&baseline.manifest)?;
    ensure!(verified.objects >= 3, "not every file was sealed: {verified:?}");
    ensure!(verified.bytes >= 40, "captured bytes undercount: {verified:?}");
    let home_tree = &baseline.manifest.mounts[HOME_GUEST_PATH];
    ensure!(home_tree.present, "an existing mount was recorded absent");
    ensure!(
        home_tree.entries["script.sh"].kind == CheckpointEntryKind::File,
        "regular file not captured as a file"
    );
    #[cfg(unix)]
    {
        ensure!(
            home_tree.entries["script.sh"].mode == Some(0o755),
            "permission bits were not captured"
        );
        ensure!(
            home_tree.entries["state-link"].kind == CheckpointEntryKind::Symlink
                && home_tree.entries["state-link"].target.as_deref()
                    == Some("nested/one/state.txt"),
            "symbolic link target was not captured"
        );
    }
    ensure!(
        home_tree.entries["nested"].kind == CheckpointEntryKind::Directory,
        "directories are not part of the closure"
    );

    // An unchanged view must not grow the journal.
    let again = store.capture("test baseline again")?;
    ensure!(!again.published, "an unchanged view was republished");
    ensure!(store.journal()?.len() == 1, "journal grew without a change");

    // Mutate the tree: overwrite, delete, add a file and an unrelated subtree.
    fs::write(home.join("nested/one/state.txt"), b"clobbered\n")?;
    fs::remove_file(home.join("script.sh"))?;
    fs::write(home.join("extra.txt"), b"extra\n")?;
    fs::create_dir_all(home.join("extra-dir/deep"))?;
    fs::write(home.join("extra-dir/deep/x"), b"x")?;

    let receipt = store.restore(&baseline.manifest, "test restore")?;
    ensure!(
        fs::read(home.join("nested/one/state.txt"))? == b"guest-state-v1\n",
        "restored bytes differ"
    );
    ensure!(
        fs::read(home.join("script.sh"))? == b"#!/bin/sh\ntrue\n",
        "deleted file was not restored"
    );
    ensure!(!home.join("extra.txt").exists(), "extra file survived the restore");
    ensure!(!home.join("extra-dir").exists(), "extra subtree survived the restore");
    ensure!(
        fs::read(memory.join("remembered.txt"))? == b"remembered\n",
        "memory bytes were not restored"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(home.join("script.sh"))?.permissions().mode() & 0o777 == 0o755,
            "restored mode differs from the captured mode"
        );
        ensure!(
            fs::read_link(home.join("state-link"))?
                == PathBuf::from("nested/one/state.txt"),
            "symbolic link was not restored"
        );
    }
    ensure!(receipt.written >= 4, "restore wrote {} entries", receipt.written);
    ensure!(
        receipt.removed == 2,
        "expected the extra file and the extra subtree to be removed, got {}",
        receipt.removed
    );
    ensure!(
        store.head()? == Some(receipt.head.clone()),
        "the restored view was not published as head"
    );
    ensure!(
        store.pending_restore()?.is_none(),
        "a completed restore left an intent behind"
    );

    // The bytes this restore replaced are retained, not destroyed.
    let safety = store.manifest(&receipt.safety)?;
    let clobbered = &safety.mounts[HOME_GUEST_PATH].entries["nested/one/state.txt"];
    ensure!(
        clobbered.digest == Digest::blake3(b"clobbered\n") && clobbered.bytes == 10,
        "the pre-restore state was not retained as a published closure"
    );

    // And a real guest sees the restored bytes through its own preopen.
    restored_bytes_are_visible_to_a_real_guest(&root)?;

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn missing_object_refuses_the_restore_before_any_mutation() -> Result<()> {
    let root = temp_dir("missing-object");
    let home = root.join("home");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(root.join("memory"))?;
    fs::write(home.join("kept.txt"), b"kept\n")?;

    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let store = store_for(&cas, &root, EntityId::new());
    let baseline = store.capture("baseline")?;
    let entry = baseline.manifest.mounts[HOME_GUEST_PATH].entries["kept.txt"].clone();

    // Remove exactly that object from the content-addressed store.
    let hex = entry
        .digest
        .canonical_blake3_hex()
        .expect("closure digest is canonical")
        .to_string();
    let object = root
        .join("cas")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(&hex);
    fs::remove_file(&object)?;

    // Mutate the home so a restore would be observable if it ran.
    fs::write(home.join("kept.txt"), b"changed\n")?;
    let error = store
        .restore(&baseline.manifest, "test")
        .expect_err("a closure with a missing object must not restore");
    let text = format!("{error:#}");
    ensure!(
        text.contains("HOME_CHECKPOINT_INCOMPLETE") && text.contains("missing or corrupt"),
        "missing object reported as: {text}"
    );
    ensure!(
        fs::read(home.join("kept.txt"))? == b"changed\n",
        "a refused restore mutated the home"
    );
    ensure!(store.pending_restore()?.is_none(), "a refused restore recorded an intent");
    ensure!(
        store.head()? == Some(baseline.id),
        "a refused restore republished the head"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[cfg(unix)]
#[test]
fn special_files_are_refused_and_absent_mounts_are_recorded_as_absent() -> Result<()> {
    use std::ffi::CString;

    let root = temp_dir("special");
    let home = root.join("home");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(root.join("memory"))?;
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let store = store_for(&cas, &root, EntityId::new());

    let fifo = home.join("pipe");
    let c_path = CString::new(fifo.to_str().expect("utf-8 fixture path"))?;
    // mkfifo is the only way to obtain a FIFO; the API derives from the real
    // entry, not from a claim about it.
    let created = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    ensure!(created == 0, "mkfifo failed: {}", std::io::Error::last_os_error());
    let error = store
        .capture("special")
        .expect_err("a special file must not be silently dropped from a closure");
    let text = format!("{error:#}");
    ensure!(
        text.contains("HOME_CHECKPOINT_UNSUPPORTED") && text.contains("special file"),
        "special file reported as: {text}"
    );
    ensure!(store.head()?.is_none(), "a refused capture published a head");
    fs::remove_file(&fifo)?;

    // An absent mount is recorded as absent, with no directory invented.
    fs::remove_dir_all(root.join("memory"))?;
    let absent = store.capture("memory absent")?;
    ensure!(absent.published, "the absent-mount closure was not published");
    ensure!(
        !absent.manifest.mounts[MEMORY_GUEST_PATH].present
            && absent.manifest.mounts[MEMORY_GUEST_PATH].entries.is_empty(),
        "an absent mount was recorded as an empty directory"
    );
    ensure!(
        !root.join("memory").exists(),
        "capture recreated the absent mount"
    );
    // Restoring a closure that recorded an absent mount would delete the mount.
    let error = store
        .restore(&absent.manifest, "test")
        .expect_err("restoring an absent mount must be refused");
    ensure!(
        format!("{error:#}").contains("restoring that absence"),
        "absent-mount restore reported as: {error:#}"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn head_journal_and_ownership_are_strict() -> Result<()> {
    let root = temp_dir("records");
    let home = root.join("home");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(root.join("memory"))?;
    fs::write(home.join("kept.txt"), b"kept\n")?;
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let installation = EntityId::new();
    let store = store_for(&cas, &root, installation);
    let baseline = store.capture("baseline")?;

    // A foreign installation cannot read this record set.
    let foreign = store_for(&cas, &root, EntityId::new());
    let error = foreign
        .latest()
        .err()
        .expect("a closure from another installation was admitted");
    ensure!(
        format!("{error:#}").contains("DATA_PATH_CONFLICT"),
        "foreign closure reported as: {error:#}"
    );

    // A corrupt head is a failure, never "no closure".
    fs::write(root.join("home-checkpoints").join("head"), b"not-a-digest\n")?;
    let error = store.head().err().expect("a corrupt head was accepted");
    ensure!(
        format!("{error:#}").contains("corrupt head"),
        "corrupt head reported as: {error:#}"
    );
    fs::write(
        root.join("home-checkpoints").join("head"),
        format!("{}\n", baseline.id),
    )?;
    ensure!(
        store.head()? == Some(baseline.id.clone()),
        "head did not name the published closure"
    );

    // An unreadable journal line is an error, not empty history.
    let journal = root.join("home-checkpoints").join("journal.jsonl");
    let mut bytes = fs::read(&journal)?;
    bytes.extend_from_slice(b"{\"format\":\"unexpected/9\"}\n");
    fs::write(&journal, &bytes)?;
    let error = store.journal().err().expect("a corrupt journal line was skipped");
    ensure!(
        format!("{error:#}").contains("HOME_CHECKPOINT_INCOMPLETE"),
        "corrupt journal reported as: {error:#}"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}
