//! Real Cargo output, a stale default-target binary, and retained observations.
//! These are compiler/receipt journeys, not a Shenko self-development acceptance.

use anyhow::{Context, Result};

use super::*;
use crate::test_evidence::FixtureEvidence;

struct Workspace(PathBuf, FixtureEvidence);

impl Workspace {
    fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("shenko-build-receipt-{}", EntityId::new()));
        std::fs::create_dir_all(&path)?;
        let mut evidence = FixtureEvidence::new(name)?;
        evidence.record("original-workspace", &serde_json::json!({ "path": path }));
        Ok(Self(path, evidence))
    }

    fn source(&mut self, label: &str, path: &Path) {
        for name in ["Cargo.toml", "Cargo.lock", "src/main.rs"] {
            self.1.file(label, &path.join(name));
        }
    }

    fn build_observation(&mut self, label: &str, result: &std::result::Result<BuildOutcome, BuildError>) {
        let outcome = match result {
            Ok(outcome) => {
                self.1.record(label, outcome);
                Some(outcome)
            }
            Err(error) => {
                self.1.record(label, &serde_json::json!({
                    "original_error": format!("{error:?}"), "outcome": error.outcome(),
                }));
                error.outcome()
            }
        };
        if let Some(artifact) = outcome.and_then(|value| value.artifact.as_ref()) {
            self.1.owned_file("actual-candidate-artifact", &artifact.path, &self.0);
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.1.enabled() || self.1.completed() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            eprintln!("incomplete build fixture originals retained at {}", self.0.display());
        }
    }
}

fn cargo() -> PathBuf {
    std::env::var_os("CARGO").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("cargo"))
}

fn project(path: &Path, package: &str, message: &str) -> Result<()> {
    std::fs::create_dir_all(path.join("src"))?;
    std::fs::write(path.join("Cargo.toml"), format!(
        "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[[bin]]\nname = \"shenko\"\npath = \"src/main.rs\"\n"))?;
    std::fs::write(path.join("src/main.rs"), format!("fn main() {{ println!(\"{message}\"); }}\n"))?;
    Ok(())
}

fn request(path: &Path, generation: &str) -> BuildRequest {
    let mut request = BuildRequest::new(path, RuntimeGeneration::new(generation));
    request.profile = BuildProfile::Debug;
    request.extra_args = vec!["--offline".into(), "--jobs=1".into()];
    request
}

fn provider(target: PathBuf) -> CargoBuildProvider {
    CargoBuildProvider {
        cargo: cargo(), target_dir: Some(target),
        limits: BuildLimits { timeout_ms: 120_000, stream_bytes: 1024 * 1024 },
    }
}

fn read_json(cas: &ContentStore, reference: &ObjectRef) -> Result<serde_json::Value> {
    let bytes = cas.read_object(&reference.digest)?;
    anyhow::ensure!(Digest::blake3(&bytes) == reference.digest && bytes.len() as u64 == reference.bytes);
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_stream(cas: &ContentStore, value: &serde_json::Value) -> Result<Vec<u8>> {
    let reference: ObjectRef = serde_json::from_value(value["raw_ref"].clone())?;
    let bytes = cas.read_object(&reference.digest)?;
    anyhow::ensure!(Digest::blake3(&bytes) == reference.digest && bytes.len() as u64 == reference.bytes);
    Ok(bytes)
}

#[test]
fn redirected_cargo_rebuild_keeps_the_actual_artifact_and_failed_attempt() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut workspace = Workspace::new("redirected-rebuild-and-failure")?;
        let source = workspace.0.join("source");
        project(&source, "receipt-candidate", "old candidate")?;
        std::fs::write(source.join("Cargo.lock"), "version = 4\n[[package]]\nname = \"receipt-candidate\"\nversion = \"0.1.0\"\n")?;
        let build_request = request(&source, "build-a");
        workspace.source("source-before-build-a", &source);
        let first_result = provider(source.join("target")).build(&build_request).await;
        workspace.build_observation("build-a", &first_result);
        let first = first_result?;
        let first_artifact = first.artifact.as_ref().context("first Cargo build has no artifact")?;
        let first_owned_path = first_artifact.path.clone();
        let old_path = source.join("target").join("debug")
            .join(first_artifact.path.file_name().context("actual binary has no filename")?);
        std::fs::create_dir_all(old_path.parent().context("default artifact parent missing")?)?;
        std::fs::copy(&first_owned_path, &old_path)?;
        let old_digest = first_artifact.digest.clone();
        let first_run = observe_command(&old_path, &[], &source, None, &probe_limits()).await;
        workspace.1.record("original-old-artifact-execution", &first_run);
        anyhow::ensure!(first_run.succeeded(), "old artifact did not execute: {first_run:?}");
        anyhow::ensure!(first_run.stdout.bytes == b"old candidate\n");

        // The old executable stays at the layout guessed by the original bug.
        // Cargo's next invocation places a different actual program elsewhere.
        project(&source, "receipt-candidate", "rebuilt candidate")?;
        let target = workspace.0.join("actual-target");
        let builder = provider(workspace.0.join("provider-root"));
        let mut second_request = request(&source, "build-b");
        second_request.extra_args.extend(["--target-dir".into(), target.to_string_lossy().into_owned()]);
        workspace.source("source-before-build-b", &source);
        let rebuilt_result = builder.build(&second_request).await;
        workspace.build_observation("build-b", &rebuilt_result);
        let rebuilt = rebuilt_result?;
        let artifact = rebuilt.artifact.as_ref().context("rebuild has no artifact")?;
        anyhow::ensure!(artifact.path.starts_with(std::fs::canonicalize(&target)?));
        anyhow::ensure!(artifact.path != old_path && artifact.digest != old_digest);
        anyhow::ensure!(Digest::blake3(&std::fs::read(&old_path)?) == old_digest);
        let reported = rebuilt.compiler_artifact.as_ref().context("compiler evidence missing")?["executable"]
            .as_str().context("reported executable missing")?;
        anyhow::ensure!(std::fs::canonicalize(reported)? == artifact.path);
        let layout = rebuilt.output_layout.as_ref().context("owned output identity missing")?;
        anyhow::ensure!(layout.argument_root_preference.as_ref() == Some(&target));
        let build_id = rebuilt.build_id.to_string();
        anyhow::ensure!(layout.owned_directory.file_name().and_then(|name| name.to_str()) == Some(build_id.as_str()));
        anyhow::ensure!(artifact.path.starts_with(&layout.owned_directory));
        anyhow::ensure!(layout.owned_directory != first.output_layout.as_ref().context("first output identity missing")?.owned_directory);
        anyhow::ensure!(Digest::blake3(&std::fs::read(&first_owned_path)?) == old_digest);
        let execution = observe_command(&artifact.path, &[], &source, None, &probe_limits()).await;
        workspace.1.record("original-rebuilt-artifact-execution", &execution);
        anyhow::ensure!(execution.succeeded(), "rebuilt artifact did not execute: {execution:?}");
        anyhow::ensure!(execution.stdout.bytes == b"rebuilt candidate\n");
        anyhow::ensure!(artifact.source_revision.is_none(), "a non-checkout cannot claim a source commit");

        let cas_dir = workspace.0.join("cas");
        let cas = ContentStore::new(cas_dir.clone())?;
        let receipt = rebuilt.seal(&cas)?;
        workspace.1.record("original-build-b-sealed-reference", &receipt);
        workspace.1.tree("original-build-b-cas", &cas_dir);
        drop(cas);
        let reopened = ContentStore::open_existing(cas_dir.clone())?;
        let saved = read_json(&reopened, &receipt)?;
        let invocation = rebuilt.invocation.as_ref().context("original invocation missing")?;
        anyhow::ensure!(read_stream(&reopened, &saved["invocation"]["stdout"])? == invocation.stdout.bytes);
        anyhow::ensure!(read_stream(&reopened, &saved["invocation"]["stderr"])? == invocation.stderr.bytes);
        anyhow::ensure!(saved["source_snapshot_frozen"] == false && saved["reproducible_build_proven"] == false);
        anyhow::ensure!(saved["validation_tests_executed_by_builder"] == false);

        // A later real compilation error retains its own failed invocation;
        // neither of the two existing executables can be substituted for it.
        std::fs::write(source.join("src/main.rs"), "fn main() { this is not Rust; }\n")?;
        workspace.source("source-before-build-c", &source);
        let failed_result = builder.build(&request(&source, "build-c")).await;
        workspace.build_observation("build-c", &failed_result);
        let failure = failed_result.err().context("invalid Rust unexpectedly built")?;
        let failed = failure.outcome().context("failed invocation was discarded")?;
        anyhow::ensure!(failed.artifact.is_none() && failed.build_id != rebuilt.build_id);
        let failed_invocation = failed.invocation.as_ref().context("failed command missing")?;
        anyhow::ensure!(failed_invocation.local_process_reaped && failed_invocation.stdout.eof && failed_invocation.stderr.eof);
        anyhow::ensure!(failed_invocation.exit_code.is_some_and(|code| code != 0));
        anyhow::ensure!(!failed_invocation.stdout.bytes.is_empty());
        let failed_ref = failed.seal(&reopened)?;
        workspace.1.record("original-build-c-sealed-reference", &failed_ref);
        workspace.1.tree("original-build-c-cas", &cas_dir);
        drop(reopened);
        let reopened = ContentStore::open_existing(cas_dir)?;
        let failed_saved = read_json(&reopened, &failed_ref)?;
        anyhow::ensure!(read_stream(&reopened, &failed_saved["invocation"]["stdout"])? == failed_invocation.stdout.bytes);
        anyhow::ensure!(read_stream(&reopened, &failed_saved["invocation"]["stderr"])? == failed_invocation.stderr.bytes);
        anyhow::ensure!(failed_saved["artifact"].is_null() && failed_saved["refusal"].is_string());
        anyhow::ensure!(Digest::blake3(&std::fs::read(&artifact.path)?) == artifact.digest);
        workspace.1.finish()?;
        Ok(())
    })
}

#[test]
fn two_real_matching_cargo_targets_are_refused_as_ambiguous() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut workspace = Workspace::new("ambiguous-real-cargo-targets")?;
        project(&workspace.0.join("a"), "candidate-a", "candidate a")?;
        project(&workspace.0.join("b"), "candidate-b", "candidate b")?;
        std::fs::write(workspace.0.join("Cargo.toml"), "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"3\"\n")?;
        std::fs::write(workspace.0.join("Cargo.lock"), "version = 4\n[[package]]\nname = \"candidate-a\"\nversion = \"0.1.0\"\n[[package]]\nname = \"candidate-b\"\nversion = \"0.1.0\"\n")?;
        for path in ["Cargo.toml", "Cargo.lock", "a/Cargo.toml", "a/src/main.rs", "b/Cargo.toml", "b/src/main.rs"] {
            workspace.1.file("original-ambiguous-source", &workspace.0.join(path));
        }
        let build_result = provider(workspace.0.join("target")).build(&request(&workspace.0, "ambiguous")).await;
        workspace.build_observation("ambiguous-build", &build_result);
        let error = build_result.err().context("two matching binary targets were accepted")?;
        let observed = error.outcome().context("ambiguous invocation was discarded")?;
        anyhow::ensure!(observed.artifact.is_none());
        anyhow::ensure!(observed.refusal.as_deref() == Some("multiple matching compiler artifacts are ambiguous"), "{error}");
        let invocation = observed.invocation.as_ref().context("original command missing")?;
        anyhow::ensure!(invocation.succeeded(), "expected real Cargo success before selection refusal: {invocation:?}");
        let messages = invocation.stdout.bytes.split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .filter(|message| message["reason"] == "compiler-artifact" && message["target"]["name"] == "shenko")
            .count();
        anyhow::ensure!(messages == 2, "actual matching compiler messages: {messages}");
        workspace.1.finish()?;
        Ok(())
    })
}

#[test]
fn overflowing_real_compiler_output_is_stopped_and_never_an_artifact() -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut workspace = Workspace::new("overflowing-real-compiler-output")?;
        project(&workspace.0, "bounded-candidate", "candidate")?;
        std::fs::write(workspace.0.join("Cargo.lock"), "version = 4\n[[package]]\nname = \"bounded-candidate\"\nversion = \"0.1.0\"\n")?;
        let mut builder = provider(workspace.0.join("target"));
        builder.limits.stream_bytes = 16;
        let original_source = workspace.0.clone();
        workspace.source("original-overflow-source", &original_source);
        let build_result = builder.build(&request(&workspace.0, "bounded")).await;
        workspace.build_observation("overflow-build", &build_result);
        let error = build_result.err().context("truncated Cargo evidence was accepted")?;
        let observed = error.outcome().context("overflow lost the original build attempt")?;
        anyhow::ensure!(observed.artifact.is_none());
        let invocation = observed.invocation.as_ref().context("overflow invocation missing")?;
        anyhow::ensure!(invocation.stdout.truncated || invocation.stderr.truncated);
        anyhow::ensure!(invocation.stdout.bytes.len() <= 16 && invocation.stderr.bytes.len() <= 16);
        anyhow::ensure!(invocation.local_process_reaped && invocation.stdout.eof && invocation.stderr.eof,
            "the original child and readers were not settled: {invocation:?}");
        anyhow::ensure!(invocation.error.is_some() && invocation.cleanup_error.is_none(), "{invocation:?}");
        workspace.1.finish()?;
        Ok(())
    })
}
