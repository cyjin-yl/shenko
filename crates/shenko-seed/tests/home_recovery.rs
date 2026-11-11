//! Recoverable first start and persistent-home loss (docs/specs/wasm-home.md#spec-8-3, docs/specs/durable-state.md#spec-10-4, C05).
//!
//! These cases run the real seed bootstrap: the sealed preview1 probe module
//! is compiled and executed against the declared preopens, host Git memory is
//! initialized by the real `git` binary, and the home manifest is the real
//! persisted file. Nothing is mocked and no host file is copied in to stand
//! in for a guest write.
//!
//! The recovery distinctions covered here:
//!
//! 1. An interrupted first start (the guest created its readiness file but the
//!    process stopped before the complete body was written) is completed by
//!    re-running this seed's own closure, and only that seed-owned artifact is
//!    rewritten.
//! 2. A home that was already declared ready and then lost a persistent
//!    directory is *not* rebuilt as an empty home: this seed holds no
//!    LKG/factory closure for those bytes, and the refusal preserves both the
//!    manifest and the absence.
//! 3. A managed directory that is a symbolic link is a conflict, never an
//!    alias into another installation's state.
//! 4. Exec admission refuses an unfinished home and classifies a missing
//!    managed directory apart from an unavailable manifest.

use anyhow::{Context, Result, ensure};
use shenko_seed::{
    Bootstrap, HOME_READY_BODY, HOME_READY_FILE, HomeManifest, HomeState, RecoveryCause, SeedConfig,
};
use shenko_types::ids::EntityId;
use std::fs;
use std::path::PathBuf;

fn fixture_root(tag: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seed-home-fixtures")
        .join(format!("recovery-{tag}-{}", EntityId::new()))
}

fn bootstrap(root: &PathBuf) -> Bootstrap {
    Bootstrap::new(SeedConfig {
        data_dir: root.clone(),
        ..Default::default()
    })
}

fn rewrite_manifest(path: &PathBuf, state: HomeState) -> Result<()> {
    let mut manifest: HomeManifest = serde_json::from_slice(&fs::read(path)?)?;
    manifest.state = state;
    fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

#[test]
fn interrupted_first_start_rewrites_only_the_seed_readiness_marker() -> Result<()> {
    let root = fixture_root("interrupted");
    let installation = EntityId::new();
    let prepared = bootstrap(&root).prepare(installation)?;
    ensure!(
        prepared.recovered.is_empty(),
        "a clean first start claimed a recovery"
    );
    let manifest_path = prepared.manifest_path.clone();
    let home_dir = prepared.manifest.home_dir().to_path_buf();
    let memory_dir = prepared.manifest.memory_dir().to_path_buf();
    let marker = home_dir.join(HOME_READY_FILE);
    ensure!(marker.is_file(), "the real guest probe left no readiness file");

    // Persistent state a recovery must never disturb: guest-visible home
    // bytes and host Git memory.
    fs::write(home_dir.join("notes.txt"), b"kept-home\n")?;
    fs::write(memory_dir.join("remembered.txt"), b"kept-memory\n")?;

    // Reconstruct exactly the interrupted first start: the manifest never
    // reached ready, and the guest stopped after creating the marker but
    // before writing the complete body.
    fs::write(&marker, &HOME_READY_BODY[..5])?;
    rewrite_manifest(&manifest_path, HomeState::Preparing)?;
    drop(prepared);

    let resumed = bootstrap(&root).prepare(installation)?;
    ensure!(
        resumed.manifest.state == HomeState::Ready,
        "resumed home did not become ready"
    );
    ensure!(
        resumed.recovered.len() == 1,
        "expected exactly one completed recovery, got {:?}",
        resumed.recovered
    );
    let recovery = &resumed.recovered[0];
    ensure!(
        recovery.cause == RecoveryCause::InterruptedGuestProbe
            && recovery.from_state == HomeState::Preparing,
        "unexpected recovery classification: {recovery:?}"
    );
    ensure!(
        recovery.rematerialized == vec![format!("/home/{HOME_READY_FILE}")],
        "recovery claimed more than the marker it rewrote: {:?}",
        recovery.rematerialized
    );
    ensure!(
        fs::read_to_string(&marker)? == HOME_READY_BODY,
        "the guest probe did not restore its exact readiness body"
    );
    ensure!(
        fs::read(home_dir.join("notes.txt"))? == b"kept-home\n"
            && fs::read(memory_dir.join("remembered.txt"))? == b"kept-memory\n",
        "recovery rewrote persistent home or memory bytes"
    );
    let persisted: HomeManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    ensure!(
        persisted == resumed.manifest,
        "recovered ready state was not persisted"
    );
    drop(resumed);
    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[test]
fn ready_home_without_its_persistent_directory_is_not_rebuilt_empty() -> Result<()> {
    let root = fixture_root("lost-home");
    let installation = EntityId::new();
    let prepared = bootstrap(&root).prepare(installation)?;
    let manifest_path = prepared.manifest_path.clone();
    let home_dir = prepared.manifest.home_dir().to_path_buf();
    let memory_dir = prepared.manifest.memory_dir().to_path_buf();
    fs::write(home_dir.join("kept.txt"), b"kept\n")?;
    drop(prepared);
    let manifest_bytes = fs::read(&manifest_path)?;

    fs::remove_dir_all(&home_dir)?;
    let error = bootstrap(&root)
        .prepare(installation)
        .err()
        .context("a ready home with a lost persistent directory was re-prepared")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("SEED_CLOSURE_INCOMPLETE")
            && text.contains("lost its persistent home directory"),
        "the loss was not reported with its real recovery boundary: {text}"
    );
    ensure!(
        !home_dir.exists(),
        "the refused recovery recreated an empty persistent home"
    );
    ensure!(
        fs::read(&manifest_path)? == manifest_bytes,
        "the refused recovery rewrote the home manifest"
    );
    ensure!(
        memory_dir.is_dir(),
        "the refusal removed the surviving memory directory"
    );
    fs::remove_dir_all(&root).ok();
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinked_managed_directory_is_a_conflict_not_an_alias() -> Result<()> {
    let root = fixture_root("symlink");
    let foreign = fixture_root("symlink-foreign");
    fs::create_dir_all(&root)?;
    let foreign_home = foreign.join("home");
    fs::create_dir_all(&foreign_home)?;
    std::os::unix::fs::symlink(&foreign_home, root.join("home"))?;

    let error = bootstrap(&root)
        .prepare(EntityId::new())
        .err()
        .context("a symlinked managed home was adopted")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("DATA_PATH_CONFLICT") && text.contains("symbolic link"),
        "a symlinked managed directory must be an explicit conflict: {text}"
    );
    ensure!(
        fs::symlink_metadata(root.join("home"))?.file_type().is_symlink(),
        "the refused bootstrap replaced the symlink"
    );
    ensure!(
        fs::read_dir(&foreign_home)?.next().is_none(),
        "the refused bootstrap wrote into the link target"
    );
    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&foreign).ok();
    Ok(())
}

#[test]
fn exec_admission_refuses_an_unfinished_home_and_names_losses() -> Result<()> {
    let root = fixture_root("exec-admission");
    let installation = EntityId::new();
    let prepared = bootstrap(&root).prepare(installation)?;
    let manifest_path = prepared.manifest_path.clone();
    let memory_dir = prepared.manifest.memory_dir().to_path_buf();
    let home_dir = prepared.manifest.home_dir().to_path_buf();
    let marker = home_dir.join(HOME_READY_FILE);
    drop(prepared);

    // A usable home is admitted.
    let admitted = shenko_seed::validate_home_for_exec(&root)?;
    ensure!(
        admitted.installation_id == installation,
        "exec admission lost the installation binding"
    );

    // A home whose preparation never finished is refused explicitly instead
    // of failing later on a missing file.
    rewrite_manifest(&manifest_path, HomeState::Preparing)?;
    let error = shenko_seed::validate_home_for_exec(&root)
        .err()
        .context("an unfinished home was admitted for exec")?;
    ensure!(
        format!("{error:#}").contains("home preparation did not complete"),
        "unfinished home reported as: {error:#}"
    );
    rewrite_manifest(&manifest_path, HomeState::Ready)?;

    // A managed directory that vanished is reported as missing, with no claim
    // that re-preparing restores it.
    fs::remove_dir_all(&memory_dir)?;
    let error = shenko_seed::validate_home_for_exec(&root)
        .err()
        .context("exec admission accepted a home with no memory directory")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("managed memory is missing"),
        "missing memory was not classified explicitly: {text}"
    );
    fs::create_dir_all(&memory_dir)?;

    // A readiness path that is no longer a regular file is refused, so a link
    // cannot stand in for this installation's readiness marker.
    #[cfg(unix)]
    {
        let elsewhere = root.join("foreign-marker");
        fs::write(&elsewhere, HOME_READY_BODY)?;
        fs::remove_file(&marker)?;
        std::os::unix::fs::symlink(&elsewhere, &marker)?;
        let error = shenko_seed::validate_home_for_exec(&root)
            .err()
            .context("exec admission followed a readiness marker link")?;
        ensure!(
            format!("{error:#}").contains("not a regular file"),
            "a linked readiness marker was not refused: {error:#}"
        );
        fs::remove_file(&marker)?;
        fs::write(&marker, HOME_READY_BODY)?;
    }
    ensure!(
        shenko_seed::validate_home_for_exec(&root).is_ok(),
        "the restored ready home must be admissible again"
    );

    // No manifest at all is a profile availability failure, not a decode
    // failure of a manifest that does not exist.
    let absent = fixture_root("exec-absent");
    fs::create_dir_all(&absent)?;
    let error = shenko_seed::validate_home_for_exec(&absent)
        .err()
        .context("exec admission accepted a directory with no home")?;
    ensure!(
        format!("{error:#}").contains("WASM_PROFILE_UNAVAILABLE")
            && format!("{error:#}").contains("no managed home manifest"),
        "absent home reported as: {error:#}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&absent).ok();
    Ok(())
}
