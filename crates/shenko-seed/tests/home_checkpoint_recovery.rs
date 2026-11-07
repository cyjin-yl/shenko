//! Real persistent-home closure recovery (SPEC §8.3, §10.1–§10.5, C05).
//!
//! These cases run the real seed bootstrap (sealed preview1 probe, real host
//! Git memory, real persisted manifest), a real content-addressed store on
//! disk, and real preview1 guests that write and read the persistent home
//! through their own preopens. No byte is copied in from the host to stand in
//! for a guest write, and no closure is fabricated.
//!
//! Covered distinctions:
//!
//! 1. A ready home that lost its persistent directory is restored from the
//!    published closure, and the restored bytes are read back by a guest.
//! 2. Damage with no published closure is reported as unrecovered; nothing is
//!    recreated empty and the manifest stays byte-identical.
//! 3. A corrupt closure refuses the restore and leaves the damage in place.
//! 4. Damage that is not file damage (a symlinked managed directory) is never
//!    "repaired" by a restore.
//! 5. An interrupted restore is resumed from its recorded intent, which names
//!    the closure to apply — not whichever closure happens to be newest.

use anyhow::{Context, Result, ensure};
use shenko_seed::{
    Bootstrap, HOME_READY_BODY, HOME_GUEST_PATH, MEMORY_GUEST_PATH, PreparedHome, RecoveryCause,
    SeedConfig,
};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use shenko_wasm::checkpoint::{CHECKPOINT_DIR, INTENT_FILE, RESTORE_INTENT_FORMAT};
use shenko_wasm::{MountSpec, WasiCommand, WasmRuntime};
use std::fs;
use std::path::{Path, PathBuf};

/// Writes `/home/guest.txt` with `guest-bytes-v1\n` through the guest preopen.
const WRITE_GUEST_FILE_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 200) "guest.txt")
  (data (i32.const 240) "guest-bytes-v1\n")
  (func (export "_start")
    (i32.store (i32.const 300) (i32.const 240))
    (i32.store (i32.const 304) (i32.const 15))
    (if (i32.ne
      (call $path_open
        (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 9)
        (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 400))
      (i32.const 0))
      (then (call $proc_exit (i32.const 60))))
    (if (i32.ne
      (call $fd_write (i32.load (i32.const 400)) (i32.const 300) (i32.const 1) (i32.const 500))
      (i32.const 0))
      (then (call $proc_exit (i32.const 61))))
    (if (i32.ne (i32.load (i32.const 500)) (i32.const 15))
      (then (call $proc_exit (i32.const 62))))
  )
)
"#;

/// Reads `/home/guest.txt` through the guest preopen and verifies the exact
/// restored bytes inside the guest itself: exit 0 only when the guest read
/// `guest-bytes-v1\n`. The verdict is the guest's own observation of the
/// preopen, not a host-side copy of the fixture.
const READ_GUEST_FILE_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 200) "guest.txt")
  (func (export "_start")
    (i32.store (i32.const 300) (i32.const 600))
    (i32.store (i32.const 304) (i32.const 64))
    (if (i32.ne
      (call $path_open
        (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 9)
        (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 400))
      (i32.const 0))
      (then (call $proc_exit (i32.const 70))))
    (if (i32.ne
      (call $fd_read (i32.load (i32.const 400)) (i32.const 300) (i32.const 1) (i32.const 500))
      (i32.const 0))
      (then (call $proc_exit (i32.const 71))))
    (if (i32.ne (i32.load (i32.const 500)) (i32.const 15))
      (then (call $proc_exit (i32.const 72))))
    (if (i32.ne (i32.load (i32.const 600)) (i32.const 0x73657567))
      (then (call $proc_exit (i32.const 80))))
    (if (i32.ne (i32.load (i32.const 604)) (i32.const 0x79622d74))
      (then (call $proc_exit (i32.const 81))))
    (if (i32.ne (i32.load (i32.const 608)) (i32.const 0x2d736574))
      (then (call $proc_exit (i32.const 82))))
    (if (i32.ne (i32.load (i32.const 612)) (i32.const 0xa3176))
      (then (call $proc_exit (i32.const 83))))
  )
)
"#;

fn fixture_root(tag: &str) -> Result<PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seed-home-fixtures")
        .join(format!("checkpoint-{tag}-{}", EntityId::new()));
    fs::create_dir_all(&root)?;
    root.canonicalize().map_err(Into::into)
}

fn bootstrap(root: &Path) -> Bootstrap {
    Bootstrap::new(SeedConfig {
        data_dir: root.to_path_buf(),
        ..Default::default()
    })
}

fn mount_specs(home: &PreparedHome) -> Vec<MountSpec> {
    home.manifest.mounts.clone()
}

fn run_guest(mounts: Vec<MountSpec>, wat: &str) -> Result<()> {
    let mut runtime = WasmRuntime::with_mounts(mounts)?;
    let outcome = runtime.run_wasi_module(
        wat.as_bytes(),
        WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty()),
    )?;
    ensure!(
        outcome == shenko_wasm::WasiRunOutcome::Exited(0),
        "guest outcome {outcome:?}"
    );
    Ok(())
}

/// Write the persistent home file through a real guest and publish the closure
/// that contains it.
fn guest_write_and_capture(
    bootstrap: &Bootstrap,
    installation: EntityId,
    cas: &ContentStore,
    root: &Path,
    reason: &str,
) -> Result<Digest> {
    let prepared = bootstrap.prepare_with_recovery(installation, cas)?;
    let mounts = mount_specs(&prepared);
    drop(prepared);
    run_guest(mounts, WRITE_GUEST_FILE_WAT)?;
    let store = checkpoint_store(cas, root, installation)?;
    let outcome = store.capture(reason)?;
    ensure!(outcome.published, "capture did not publish a closure");
    Ok(outcome.id)
}

fn checkpoint_store<'a>(
    cas: &'a ContentStore,
    root: &'a Path,
    installation: EntityId,
) -> Result<shenko_wasm::checkpoint::HomeCheckpointStore<'a>> {
    shenko_wasm::checkpoint::HomeCheckpointStore::new(
        cas,
        root,
        installation,
        vec![
            (HOME_GUEST_PATH.to_string(), root.join("home")),
            (MEMORY_GUEST_PATH.to_string(), root.join("memory")),
        ],
    )
}

#[test]
fn lost_persistent_home_is_restored_from_the_published_closure() -> Result<()> {
    let root = fixture_root("lost-home")?;
    let installation = EntityId::new();
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let bootstrap = bootstrap(&root);

    let baseline = bootstrap.prepare_with_recovery(installation, &cas)?;
    ensure!(
        baseline.recovered.is_empty(),
        "a clean first start claimed a recovery: {:?}",
        baseline.recovered
    );
    let created_at = baseline.manifest.created_at;
    let manifest_path = baseline.manifest_path.clone();
    let specs_before = baseline.manifest.mounts.clone();
    let manifests_before = fs::read(&manifest_path)?;
    drop(baseline);
    let store = checkpoint_store(&cas, &root, installation)?;
    ensure!(
        store.head()?.is_some(),
        "first start published no byte closure"
    );

    // Real guest bytes, then a published closure containing them.
    let captured = guest_write_and_capture(
        &bootstrap,
        installation,
        &cas,
        &root,
        "after guest write",
    )?;
    let manifest = store.manifest(&captured)?;
    let entry = &manifest.mounts[HOME_GUEST_PATH].entries["guest.txt"];
    ensure!(
        entry.digest == Digest::blake3(b"guest-bytes-v1\n") && entry.bytes == 15,
        "the closure does not describe the guest's file"
    );

    // The persistent home directory is lost; nothing else is damaged.
    let home_dir = root.join("home");
    fs::remove_dir_all(&home_dir)?;
    ensure!(!home_dir.exists(), "damage fixture did not remove the home");

    let recovered = bootstrap
        .prepare_with_recovery(installation, &cas)
        .context("recovery from a verified closure failed")?;
    ensure!(
        recovered.recovered.len() == 1,
        "expected one completed recovery, got {:?}",
        recovered.recovered
    );
    let recovery = &recovered.recovered[0];
    match &recovery.cause {
        RecoveryCause::CheckpointRestore {
            checkpoint, written, ..
        } => {
            ensure!(
                *checkpoint == captured,
                "recovery restored {checkpoint}, not the published closure {captured}"
            );
            ensure!(*written >= 2, "restore wrote {written} entries");
        }
        other => panic!("unexpected recovery cause {other:?}"),
    }
    ensure!(
        recovery.rematerialized == vec![HOME_GUEST_PATH.to_string(), MEMORY_GUEST_PATH.to_string()],
        "recovery named the wrong mounts: {:?}",
        recovery.rematerialized
    );

    // Host bytes, the sealed seed marker and the installation identity all
    // survive the restore.
    ensure!(
        fs::read(home_dir.join("guest.txt"))? == b"guest-bytes-v1\n",
        "restored guest bytes differ"
    );
    ensure!(
        fs::read_to_string(home_dir.join(shenko_seed::HOME_READY_FILE))? == HOME_READY_BODY,
        "the restored home lost its guest readiness marker"
    );
    ensure!(
        recovered.manifest.installation_id == installation
            && recovered.manifest.created_at == created_at
            && recovered.manifest.mounts == specs_before,
        "recovery changed the installation, mounts or original creation time"
    );
    // The sealed home manifest is not rewritten by a restore: recovery repairs
    // the file tree it declares, not the declaration itself.
    ensure!(
        fs::read(&manifest_path)? == manifests_before,
        "recovery rewrote the home manifest"
    );

    // The restored bytes are what a guest reads through its own preopen.
    let mounts = mount_specs(&recovered);
    drop(recovered);
    run_guest(mounts, READ_GUEST_FILE_WAT)
        .context("the guest preopen did not serve the exact restored bytes")?;
    ensure!(
        store.pending_restore()?.is_none(),
        "a completed restore left an intent behind"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn damage_without_a_published_closure_stays_unrecovered() -> Result<()> {
    let root = fixture_root("no-closure")?;
    let installation = EntityId::new();
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let bootstrap = bootstrap(&root);

    // Prepare without the checkpoint path: no closure is ever published.
    let prepared = bootstrap.prepare(installation)?;
    let manifest_path = prepared.manifest_path.clone();
    drop(prepared);
    let manifests = fs::read(&manifest_path)?;
    let home_dir = root.join("home");
    fs::remove_dir_all(&home_dir)?;

    let error = bootstrap
        .prepare_with_recovery(installation, &cas)
        .err()
        .context("damage with no closure was reported as recovered")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("no published home checkpoint closure")
            && text.contains("lost its persistent home directory"),
        "unrecoverable damage reported as: {text}"
    );
    ensure!(
        !home_dir.exists(),
        "the refused recovery recreated an empty persistent home"
    );
    ensure!(
        fs::read(&manifest_path)? == manifests,
        "the refused recovery rewrote the home manifest"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn corrupt_closure_refuses_the_restore_and_keeps_the_damage() -> Result<()> {
    let root = fixture_root("corrupt")?;
    let installation = EntityId::new();
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let bootstrap = bootstrap(&root);

    let baseline = bootstrap.prepare_with_recovery(installation, &cas)?;
    drop(baseline);
    let store = checkpoint_store(&cas, &root, installation)?;
    let head = store.head()?.context("no published closure")?;

    // Remove the object that holds the seed's own readiness marker.
    let marker = Digest::blake3(HOME_READY_BODY.as_bytes());
    let hex = marker
        .canonical_blake3_hex()
        .context("marker digest is canonical")?
        .to_string();
    let object = root
        .join("cas")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(&hex);
    fs::remove_file(&object)?;

    let home_dir = root.join("home");
    fs::remove_dir_all(&home_dir)?;
    let error = bootstrap
        .prepare_with_recovery(installation, &cas)
        .err()
        .context("a closure with a missing object was restored")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("HOME_CHECKPOINT_INCOMPLETE") && text.contains("missing or corrupt"),
        "corrupt closure reported as: {text}"
    );
    ensure!(
        !home_dir.exists(),
        "the refused restore created a partial persistent home"
    );
    ensure!(
        store.head()? == Some(head),
        "a refused restore republished the head"
    );
    ensure!(
        store.pending_restore()?.is_none(),
        "a refused restore recorded an intent"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlinked_managed_directory_is_never_repaired_by_a_restore() -> Result<()> {
    let root = fixture_root("symlink")?;
    let foreign = fixture_root("symlink-foreign")?;
    let installation = EntityId::new();
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let bootstrap = bootstrap(&root);

    let baseline = bootstrap.prepare_with_recovery(installation, &cas)?;
    drop(baseline);
    let store = checkpoint_store(&cas, &root, installation)?;
    ensure!(store.head()?.is_some(), "no closure to recover from");

    let home_dir = root.join("home");
    fs::remove_dir_all(&home_dir)?;
    let foreign_home = foreign.join("home");
    fs::create_dir_all(&foreign_home)?;
    std::os::unix::fs::symlink(&foreign_home, &home_dir)?;

    let error = bootstrap
        .prepare_with_recovery(installation, &cas)
        .err()
        .context("a symlinked managed directory was repaired instead of refused")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("DATA_PATH_CONFLICT") && text.contains("symbolic link"),
        "symlinked damage reported as: {text}"
    );
    ensure!(
        fs::symlink_metadata(&home_dir)?.file_type().is_symlink(),
        "the refused recovery replaced the symlink"
    );
    ensure!(
        fs::read_dir(&foreign_home)?.next().is_none(),
        "the refused recovery wrote into the link target"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&foreign).ok();
    Ok(())
}

#[test]
fn an_interrupted_restore_resumes_the_closure_its_intent_names() -> Result<()> {
    let root = fixture_root("resume")?;
    let installation = EntityId::new();
    let cas = ContentStore::new(root.join("cas")).expect("cas");
    let bootstrap = bootstrap(&root);

    let baseline = bootstrap.prepare_with_recovery(installation, &cas)?;
    drop(baseline);
    let store = checkpoint_store(&cas, &root, installation)?;
    let v1 = store.head()?.context("no first closure")?;

    // A later closure that also contains an extra file, published as head.
    let v2 = guest_write_and_capture(&bootstrap, installation, &cas, &root, "v2")?;
    ensure!(v2 != v1, "the second capture did not change the view");

    // Damage the home and record an intent that names v1, not the newest v2.
    let home_dir = root.join("home");
    fs::remove_dir_all(&home_dir)?;
    let intent = serde_json::json!({
        "format": RESTORE_INTENT_FORMAT,
        "checkpoint": v1,
        "safety": v2,
        "timestamp_unix_ms": 0u64,
    });
    let record_dir = root.join(CHECKPOINT_DIR);
    fs::write(
        record_dir.join(INTENT_FILE),
        serde_json::to_vec_pretty(&intent)?,
    )?;

    let recovered = bootstrap
        .prepare_with_recovery(installation, &cas)
        .context("the interrupted restore was not resumed")?;
    let recovery = recovered
        .recovered
        .first()
        .context("resumed restore was not reported")?;
    match &recovery.cause {
        RecoveryCause::CheckpointRestore { checkpoint, .. } => ensure!(
            *checkpoint == v1,
            "resume applied {checkpoint} instead of the intent's {v1}"
        ),
        other => panic!("unexpected recovery cause {other:?}"),
    }
    // v1 predates the guest write, so the file only v2 knew about is gone and
    // the seed marker from v1 is back.
    ensure!(
        !home_dir.join("guest.txt").exists(),
        "resume applied the newest closure instead of the recorded intent"
    );
    ensure!(
        fs::read_to_string(home_dir.join(shenko_seed::HOME_READY_FILE))? == HOME_READY_BODY,
        "resumed restore did not recreate the seed readiness marker"
    );
    ensure!(
        store.pending_restore()?.is_none(),
        "the resumed restore left its intent behind"
    );

    fs::remove_dir_all(&root).ok();
    Ok(())
}
