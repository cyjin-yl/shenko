//! Declared-mount admission failures (docs/specs/wasm-home.md#spec-8-3, C05).
//!
//! A declared mount either resolves to a directory this process can enter, or
//! admission fails with the cause preserved. Missing, inaccessible,
//! symbolic-link and non-directory sources are distinct outcomes: they need
//! different actions, and none of them is repaired by creating a directory,
//! substituting another host path, or following a link. Admission also never
//! creates managed workspace state for a mount it could not enter.
//!
//! Every case here is a real host filesystem and the real public API
//! (`MountSpec::source_state`/`validate`, `WasmRuntime::with_mounts`,
//! `admit_mounts`); nothing is mocked.

use anyhow::Result;
use shenko_wasm::{
    HOME_GUEST_PATH, MountMode, MountSourceState, MountSpec, WasiCommand, WasmRuntime,
};
use std::fs;
use std::path::{Path, PathBuf};

fn temp_dir(tag: &str) -> PathBuf {
    let root = std::env::var_os("SHENKO_WASM_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!(
        "shenko-wasm-admission-{tag}-{}",
        shenko_types::ids::EntityId::new()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn no_stdio() -> WasiCommand<std::io::Empty, std::io::Empty, std::io::Empty> {
    WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty())
}

fn classify(source: &Path) -> MountSourceState {
    MountSpec::new(source.to_path_buf(), "/probe", MountMode::ReadOnly, false, false)
        .source_state()
}

fn failure(mount: MountSpec) -> String {
    let error = mount
        .validate()
        .expect_err("a declared mount with no enterable source must fail admission");
    format!("{error:#}")
}

/// Missing, non-directory and symbolic-link sources are classified apart and
/// nothing is created for them: a declared mount is never satisfied by
/// creating the directory it names.
#[test]
fn declared_sources_are_classified_by_cause_and_never_created() -> Result<()> {
    let root = temp_dir("classify");

    let real = root.join("real");
    fs::create_dir_all(&real)?;
    match classify(&real) {
        MountSourceState::Ready => {}
        other => panic!("an existing directory classified as {other:?}"),
    }
    assert!(
        MountSpec::new(real.clone(), "/real", MountMode::ReadWrite, true, false)
            .validate()
            .is_ok(),
        "an existing directory must still be admissible"
    );

    let missing = root.join("missing");
    match classify(&missing) {
        MountSourceState::Missing => {}
        other => panic!("a deleted declared source classified as {other:?}"),
    }
    let text = failure(MountSpec::new(
        missing.clone(),
        "/absent",
        MountMode::ReadWrite,
        true,
        false,
    ));
    assert!(
        text.contains("WASM_PROFILE_UNAVAILABLE") && text.contains("does not exist"),
        "a missing declared mount must name its cause: {text}"
    );
    assert!(
        !missing.exists(),
        "admission created the directory a declared mount named"
    );

    let regular_file = root.join("regular-file");
    fs::write(&regular_file, b"not-a-directory")?;
    match classify(&regular_file) {
        MountSourceState::NotADirectory => {}
        other => panic!("a regular file classified as {other:?}"),
    }
    assert!(
        failure(MountSpec::new(
            regular_file,
            "/file",
            MountMode::ReadOnly,
            false,
            false
        ))
        .contains("is not a directory"),
        "a non-directory source must be reported as such"
    );

    #[cfg(unix)]
    {
        let target = root.join("link-target");
        fs::create_dir_all(&target)?;
        let link = root.join("link-to-dir");
        std::os::unix::fs::symlink(&target, &link)?;
        let expected_target = target.canonicalize()?;
        match classify(&link) {
            MountSourceState::SymbolicLink { resolves_to } => {
                assert_eq!(
                    resolves_to.as_deref(),
                    Some(expected_target.as_path()),
                    "a resolving link must report its target"
                );
            }
            other => panic!("a symbolic link classified as {other:?}"),
        }
        let text = failure(MountSpec::new(
            link.clone(),
            "/link",
            MountMode::ReadWrite,
            true,
            false,
        ));
        assert!(
            text.contains("WASM_PROFILE_UNAVAILABLE") && text.contains("symbolic link"),
            "a linked source must not be admitted through its target: {text}"
        );

        let dangling = root.join("dangling-link");
        std::os::unix::fs::symlink(root.join("never-created"), &dangling)?;
        match classify(&dangling) {
            MountSourceState::SymbolicLink { resolves_to: None } => {}
            other => panic!("a dangling symbolic link classified as {other:?}"),
        }
        assert!(
            failure(MountSpec::new(
                dangling,
                "/dangling",
                MountMode::ReadOnly,
                false,
                false
            ))
            .contains("dangling"),
            "a dangling link must be reported as dangling"
        );
    }

    fs::remove_dir_all(&root).ok();
    Ok(())
}

/// A directory this process may not enter is inadmissible and reported as
/// inaccessible, not as missing and not as a successful mount.
///
/// The mode-bit case only exercises `EACCES` when the test process is not
/// privileged enough to bypass permission bits; under such a uid the probe
/// detects that and the admission case is not claimed.
#[cfg(unix)]
#[test]
fn unenterable_declared_source_is_reported_as_inaccessible() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_dir("inaccessible");
    let locked = root.join("locked");
    fs::create_dir_all(&locked)?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    let privileged = fs::read_dir(&locked).is_ok();

    if !privileged {
        let home = root.join("home");
        fs::create_dir_all(&home)?;
        let error = shenko_wasm::admit_mounts(
            &[
                MountSpec::new(home.clone(), HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
                MountSpec::new(locked.clone(), "/locked", MountMode::ReadOnly, false, false),
            ],
            &home,
        )
        .map(|_| ())
        .expect_err("admission of an unenterable mount succeeded");
        let text = format!("{error:#}");
        assert!(
            text.contains("WASM_PROFILE_UNAVAILABLE") && text.contains("inaccessible"),
            "admission must retain the inaccessible-source failure: {text}"
        );
    }

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))?;
    fs::remove_dir_all(&root).ok();
    Ok(())
}

/// Runtime construction refuses every inadmissible declared mount and leaves
/// the host untouched — the runtime is never built over a substituted path.
#[test]
fn runtime_construction_refuses_a_bad_declared_mount() -> Result<()> {
    let home = temp_dir("runtime-home");
    let missing = home.join("gone-after-declaration");
    let error = WasmRuntime::with_mounts(vec![
        MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        ),
        MountSpec::new(
            missing.clone(),
            "/gone",
            MountMode::ReadWrite,
            false,
            false,
        ),
    ])
    .map(|_| ())
    .expect_err("a runtime over a missing declared mount must not be constructed");
    let text = format!("{error:#}");
    assert!(
        text.contains("WASM_PROFILE_UNAVAILABLE"),
        "explicit typed failure expected, got: {text}"
    );
    assert!(!missing.exists(), "runtime construction created a mount source");

    fs::remove_dir_all(&home).ok();
    Ok(())
}

/// A COW mount whose base cannot be entered fails admission without creating
/// workspace, branch, overlay or tombstone state under the home: managed
/// state is only materialized for a mount that was actually admitted.
#[cfg(unix)]
#[test]
fn refused_cow_admission_leaves_no_managed_workspace_state() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let home = temp_dir("cow-home");
    let base = temp_dir("cow-base");
    fs::write(base.join("keep.txt"), b"base-keep\n")?;

    // A declared base that disappears after declaration: validation at runtime
    // construction succeeds, the run's own admission refuses it.
    let mut runtime = WasmRuntime::with_mounts(vec![
        MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        ),
        MountSpec::cow(base.clone(), "/base", "main", "isolation", true, true),
    ])?;
    fs::remove_dir_all(&base)?;
    let error = runtime
        .run_wasi_module(
            &wat::parse_str("(module (func (export \"_start\")))").expect("fixture module"),
            no_stdio(),
        )
        .map(|_| ())
        .expect_err("a run over a vanished COW base must fail");
    assert!(
        format!("{error:#}").contains("WASM_PROFILE_UNAVAILABLE"),
        "explicit typed failure expected, got: {error:#}"
    );
    let branch = home.join("workspaces/main/branches/isolation");
    assert!(
        !branch.exists(),
        "failed COW admission materialized {}",
        branch.display()
    );

    // The same invariant when the base exists but cannot be entered: the
    // declaration validates (stat needs only the parent), admission cannot
    // open it. Skipped when this process bypasses mode bits.
    let locked = temp_dir("cow-locked-base");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    if fs::read_dir(&locked).is_err() {
        let locked_home = temp_dir("cow-locked-home");
        let error = shenko_wasm::admit_mounts(
            &[
                MountSpec::new(
                    locked_home.clone(),
                    HOME_GUEST_PATH,
                    MountMode::ReadWrite,
                    true,
                    false,
                ),
                MountSpec::cow(locked.clone(), "/base", "main", "locked", true, true),
            ],
            &locked_home,
        )
        .map(|_| ())
        .expect_err("admission of an unenterable COW base succeeded");
        assert!(
            format!("{error:#}").contains("WASM_PROFILE_UNAVAILABLE"),
            "explicit typed failure expected, got: {error:#}"
        );
        let branch = locked_home.join("workspaces/main/branches/locked");
        assert!(
            !branch.exists(),
            "failed COW base admission materialized {}",
            branch.display()
        );
        fs::remove_dir_all(&locked_home).ok();
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))?;

    fs::remove_dir_all(&locked).ok();
    fs::remove_dir_all(&home).ok();
    Ok(())
}
