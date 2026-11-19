//! Candidate builds bind the artifact reported by their own Cargo invocation.
//! Checkout metadata is an observation, not an immutable source snapshot.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use shenko_store::cas::ContentStore;
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::object::{Digest, ObjectRef};

use crate::smoke::{CleanupNotes, OwnedTree};

const OUTPUT_TAIL_BYTES: usize = 8 * 1024;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const PROBE_OUTPUT_BYTES: usize = 128 * 1024;
const RETIRE_JOIN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildProfile {
    Debug,
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    pub source_dir: PathBuf,
    pub generation: RuntimeGeneration,
    pub profile: BuildProfile,
    pub target: Option<String>,
    pub extra_args: Vec<String>,
    pub binary_name: String,
}

impl BuildRequest {
    pub fn new(source_dir: impl Into<PathBuf>, generation: RuntimeGeneration) -> Self {
        Self {
            source_dir: source_dir.into(), generation, profile: BuildProfile::Release,
            target: None, extra_args: Vec::new(), binary_name: "shenko".into(),
        }
    }
}

/// Finite collection limits of this build provider, recorded in each attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildLimits {
    pub timeout_ms: u64,
    pub stream_bytes: usize,
}

impl Default for BuildLimits {
    fn default() -> Self {
        Self { timeout_ms: 30 * 60 * 1000, stream_bytes: 16 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateArtifact {
    pub generation: RuntimeGeneration,
    pub path: PathBuf,
    pub digest: Digest,
    pub bytes: u64,
    /// Equal, clean checkout HEAD observations before and after this build.
    /// Even this does not attest ignored files, environment or immutable inputs.
    pub source_revision: Option<String>,
    pub lockfile_digest: Option<Digest>,
    pub manifest_digest: Option<Digest>,
    /// Cargo's observed version in the build directory, not a compiler attestation.
    pub toolchain: String,
    /// The requested profile and target; the receipt also retains Cargo's actual
    /// compiler profile and exact arguments, including additional arguments.
    pub profile: BuildProfile,
    pub target: Option<String>,
    pub built_at: String,
}

/// Exact retained bytes. Overflow is a failed observation, never a valid build
/// whose compiler messages were silently discarded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapturedStream {
    pub bytes: Vec<u8>,
    pub observed_bytes: u64,
    pub eof: bool,
    pub truncated: bool,
    pub error: Option<String>,
}

impl CapturedStream {
    fn append(&mut self, bytes: &[u8], limit: usize) {
        self.observed_bytes = self.observed_bytes.saturating_add(bytes.len() as u64);
        let retained = limit.saturating_sub(self.bytes.len()).min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..retained]);
        self.truncated |= retained != bytes.len();
    }

    fn complete(&self) -> bool { self.eof && !self.truncated && self.error.is_none() }

    fn seal(&self, cas: &ContentStore, name: &str) -> anyhow::Result<serde_json::Value> {
        let object = cas.seal_object(&self.bytes, "application/octet-stream", "activation", name, 1)?.object;
        Ok(serde_json::json!({
            "raw_ref": object, "observed_bytes": self.observed_bytes,
            "eof": self.eof, "truncated": self.truncated, "error": self.error,
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandObservation {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub explicit_target_dir: Option<PathBuf>,
    pub limits: BuildLimits,
    pub started_at: String,
    pub finished_at: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub local_process_reaped: bool,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
    pub error: Option<String>,
    pub cleanup_error: Option<String>,
}

impl CommandObservation {
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && self.local_process_reaped && self.error.is_none()
            && self.cleanup_error.is_none() && self.stdout.complete() && self.stderr.complete()
    }

    fn seal(&self, cas: &ContentStore) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "command": self.command, "cwd": self.cwd, "explicit_target_dir": self.explicit_target_dir,
            "limits": self.limits, "started_at": self.started_at, "finished_at": self.finished_at,
            "pid": self.pid, "exit_code": self.exit_code, "local_process_reaped": self.local_process_reaped,
            "stdout": self.stdout.seal(cas, "build-stdout")?,
            "stderr": self.stderr.seal(cas, "build-stderr")?,
            "error": self.error, "cleanup_error": self.cleanup_error,
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileObservation {
    pub path: PathBuf,
    pub digest: Option<Digest>,
    pub bytes: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceObservation {
    pub head: CommandObservation,
    pub status: CommandObservation,
    pub manifest: FileObservation,
    pub lockfile: FileObservation,
}

impl SourceObservation {
    fn clean_head(&self) -> Option<String> {
        if !self.head.succeeded() || !self.status.succeeded() || !self.status.stdout.bytes.is_empty() {
            return None;
        }
        let head = std::str::from_utf8(&self.head.stdout.bytes).ok()?.trim();
        ((head.len() == 40 || head.len() == 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| head.to_owned())
    }

    fn seal(&self, cas: &ContentStore) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "head": self.head.seal(cas)?, "status": self.status.seal(cas)?,
            "manifest": self.manifest, "lockfile": self.lockfile,
            "source_snapshot_frozen": false,
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutputLayout {
    pub provider_root_preference: Option<PathBuf>,
    pub environment_root_preference: Option<PathBuf>,
    pub argument_root_preference: Option<PathBuf>,
    pub cargo_metadata_root: PathBuf,
    pub canonical_preferred_root: PathBuf,
    pub owned_directory: PathBuf,
}

/// One original observation. Cargo artifacts require their own complete
/// successful compiler stream. Prebuilt inputs explicitly have no invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutcome {
    pub build_id: EntityId,
    pub request: Option<BuildRequest>,
    pub artifact: Option<CandidateArtifact>,
    pub exit_code: i32,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub command: Vec<String>,
    pub invocation: Option<CommandObservation>,
    pub cargo_version: Option<CommandObservation>,
    pub cargo_metadata: Option<CommandObservation>,
    pub output_layout: Option<BuildOutputLayout>,
    pub source_before: Option<SourceObservation>,
    pub source_after: Option<SourceObservation>,
    pub compiler_artifact: Option<serde_json::Value>,
    pub refusal: Option<String>,
}

impl BuildOutcome {
    fn new(request: BuildRequest, command: Vec<String>) -> Self {
        Self {
            build_id: EntityId::new(), request: Some(request), artifact: None, exit_code: -1,
            stdout_tail: String::new(), stderr_tail: String::new(), command,
            invocation: None, cargo_version: None, cargo_metadata: None, output_layout: None,
            source_before: None, source_after: None,
            compiler_artifact: None, refusal: None,
        }
    }

    pub fn prebuilt(path: &Path, generation: &RuntimeGeneration) -> Result<Self, BuildError> {
        let mut outcome = Self::new(BuildRequest::new(PathBuf::new(), generation.clone()),
            vec![path.to_string_lossy().into_owned()]);
        outcome.request = None;
        outcome.artifact = Some(match describe_artifact(path, generation) {
            Ok(artifact) => artifact,
            Err(error) => return Err(outcome.refuse(format!("prebuilt artifact cannot be read: {error}"))),
        });
        outcome.exit_code = 0;
        Ok(outcome)
    }

    fn refuse(mut self, reason: impl Into<String>) -> BuildError {
        let reason = reason.into();
        self.refusal = Some(reason.clone());
        BuildError::Attempt { reason, outcome: Box::new(self) }
    }

    /// Seal original streams separately so no UTF-8 replacement or inline
    /// transcript limit can change their bytes. The caller roots this JSON in
    /// an activation closure or the existing durable error journal.
    pub fn seal(&self, cas: &ContentStore) -> anyhow::Result<ObjectRef> {
        let body = serde_json::json!({
            "schema_version": 1, "build_id": self.build_id, "request": self.request,
            "artifact": self.artifact, "exit_code": self.exit_code, "command": self.command,
            "stdout_tail": self.stdout_tail, "stderr_tail": self.stderr_tail,
            "invocation": self.invocation.as_ref().map(|value| value.seal(cas)).transpose()?,
            "cargo_version": self.cargo_version.as_ref().map(|value| value.seal(cas)).transpose()?,
            "cargo_metadata": self.cargo_metadata.as_ref().map(|value| value.seal(cas)).transpose()?,
            "output_layout": self.output_layout,
            "source_before": self.source_before.as_ref().map(|value| value.seal(cas)).transpose()?,
            "source_after": self.source_after.as_ref().map(|value| value.seal(cas)).transpose()?,
            "compiler_artifact": self.compiler_artifact, "refusal": self.refusal,
            "source_snapshot_frozen": false, "reproducible_build_proven": false,
            "validation_tests_executed_by_builder": false,
            "prebuilt_content_observation": self.request.is_none(),
        });
        Ok(cas.seal_object(&serde_json::to_vec(&body)?, "application/json", "activation", "build-receipt", 1)?.object)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("candidate build failed: {reason}")]
    Attempt { reason: String, outcome: Box<BuildOutcome> },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl BuildError {
    pub fn outcome(&self) -> Option<&BuildOutcome> {
        match self { Self::Attempt { outcome, .. } => Some(outcome), Self::Io(_) => None }
    }
}

#[async_trait::async_trait]
pub trait BuildProvider: Send + Sync {
    async fn build(&self, request: &BuildRequest) -> Result<BuildOutcome, BuildError>;
}

#[derive(Debug, Clone)]
pub struct CargoBuildProvider {
    pub cargo: PathBuf,
    /// Root preference; each build uses its own newly created child directory.
    pub target_dir: Option<PathBuf>,
    pub limits: BuildLimits,
}

impl Default for CargoBuildProvider {
    fn default() -> Self { Self::new("cargo") }
}

impl CargoBuildProvider {
    pub fn new(cargo: impl Into<PathBuf>) -> Self {
        Self { cargo: cargo.into(), target_dir: None, limits: BuildLimits::default() }
    }
}

#[async_trait::async_trait]
impl BuildProvider for CargoBuildProvider {
    async fn build(&self, request: &BuildRequest) -> Result<BuildOutcome, BuildError> {
        let mut args = vec!["build".to_owned(), "--locked".to_owned(),
            "--message-format=json-render-diagnostics".to_owned(), "--bin".to_owned(), request.binary_name.clone()];
        if request.profile == BuildProfile::Release { args.push("--release".into()); }
        if let Some(target) = &request.target { args.extend(["--target".into(), target.clone()]); }
        args.extend(request.extra_args.iter().cloned());
        let mut command_line = vec![self.cargo.to_string_lossy().into_owned()];
        command_line.extend(args.iter().cloned());
        let mut outcome = BuildOutcome::new(request.clone(), command_line);
        let extras = match normalized_arguments(&request.extra_args) {
            Ok(extras) => extras,
            Err(error) => return Err(outcome.refuse(error)),
        };
        if self.limits.timeout_ms == 0 || self.limits.stream_bytes == 0 {
            return Err(outcome.refuse("build collection limits must be positive"));
        }
        let source = match std::fs::canonicalize(&request.source_dir) {
            Ok(path) => path,
            Err(error) => return Err(outcome.refuse(format!("source directory unavailable: {error}"))),
        };
        outcome.source_before = Some(observe_source(&source).await);
        outcome.cargo_version = Some(observe_command(&self.cargo, &["--version".into(), "--verbose".into()],
            &source, None, &probe_limits()).await);
        let environment_root = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
        let metadata_root_preference = self.target_dir.as_deref().or(environment_root.as_deref());
        let mut metadata_args = vec!["metadata".into(), "--no-deps".into(), "--format-version=1".into(), "--locked".into()];
        metadata_args.extend(extras.metadata.iter().cloned());
        let metadata_observation = observe_command(&self.cargo, &metadata_args, &source,
            metadata_root_preference, &BuildLimits { timeout_ms: 15_000, stream_bytes: 4 * 1024 * 1024 }).await;
        outcome.cargo_metadata = Some(metadata_observation);
        let metadata = match outcome.cargo_metadata.as_ref().filter(|observation| observation.succeeded()) {
            Some(observation) => match serde_json::from_slice::<serde_json::Value>(&observation.stdout.bytes) {
                Ok(metadata) => metadata,
                Err(error) => return Err(outcome.refuse(format!("Cargo metadata is not valid JSON: {error}"))),
            },
            None => return Err(outcome.refuse("Cargo target-root metadata did not complete successfully; see its original observation")),
        };
        if metadata["version"].as_u64() != Some(1) {
            return Err(outcome.refuse("Cargo metadata format version is not 1"));
        }
        let layout = match owned_output_layout(outcome.build_id, &source, self.target_dir.as_deref(),
            environment_root.as_deref(), extras.target_root.as_deref(), &metadata) {
            Ok(layout) => layout,
            Err(error) => return Err(outcome.refuse(format!("owned build output cannot be prepared: {error}"))),
        };
        let owned = layout.owned_directory.clone();
        outcome.output_layout = Some(layout);
        let owned_arg = match owned.to_str() {
            Some(path) => path.to_owned(),
            None => return Err(outcome.refuse("owned output path is not representable in Cargo JSON evidence")),
        };
        // Original extra arguments remain in the request. Only the explicit
        // root preference is replaced by this attempt's unique output tree.
        args.truncate(args.len() - request.extra_args.len());
        args.extend(extras.build);
        args.extend(["--target-dir".into(), owned_arg]);
        outcome.command = vec![self.cargo.to_string_lossy().into_owned()];
        outcome.command.extend(args.iter().cloned());
        let invocation = observe_command(&self.cargo, &args, &source, Some(&owned), &self.limits).await;
        outcome.exit_code = invocation.exit_code.unwrap_or(-1);
        outcome.stdout_tail = tail(&String::from_utf8_lossy(&invocation.stdout.bytes));
        outcome.stderr_tail = tail(&String::from_utf8_lossy(&invocation.stderr.bytes));
        outcome.invocation = Some(invocation);
        outcome.source_after = Some(observe_source(&source).await);
        let invocation = match outcome.invocation.as_ref() {
            Some(value) => value,
            None => return Err(outcome.refuse("build invocation observation is missing")),
        };
        if !invocation.succeeded() {
            let reason = format!("Cargo did not complete successfully: exit={:?}, observation={:?}, cleanup={:?}; {}",
                invocation.exit_code, invocation.error, invocation.cleanup_error, outcome.stderr_tail);
            return Err(outcome.refuse(reason));
        }
        let message = match select_artifact(&invocation.stdout.bytes, request, &source, &owned, &metadata) {
            Ok(value) => value,
            Err(reason) => return Err(outcome.refuse(reason)),
        };
        let path = match message.get("executable").and_then(serde_json::Value::as_str) {
            Some(path) => match std::fs::canonicalize(path) {
                Ok(path) => path,
                Err(error) => return Err(outcome.refuse(format!("reported executable disappeared: {error}"))),
            },
            None => return Err(outcome.refuse("the selected compiler message has no executable")),
        };
        outcome.compiler_artifact = Some(message);
        let mut artifact = match describe_artifact(&path, &request.generation) {
            Ok(value) => value,
            Err(error) => return Err(outcome.refuse(format!("the reported artifact cannot be read: {error}"))),
        };
        artifact.profile = request.profile;
        artifact.target = request.target.clone();
        artifact.toolchain = outcome.cargo_version.as_ref().filter(|value| value.succeeded())
            .map(|value| String::from_utf8_lossy(&value.stdout.bytes).trim().to_owned())
            .unwrap_or_else(|| "cargo version unobserved; see original probe".into());
        if let (Some(before), Some(after)) = (&outcome.source_before, &outcome.source_after) {
            if before.clean_head() == after.clean_head() { artifact.source_revision = before.clean_head(); }
            let observed_workspace = metadata["workspace_root"].as_str()
                .and_then(|path| std::fs::canonicalize(path).ok()).as_ref() == Some(&source);
            if observed_workspace {
                if before.lockfile.digest == after.lockfile.digest { artifact.lockfile_digest = before.lockfile.digest.clone(); }
                if before.manifest.digest == after.manifest.digest { artifact.manifest_digest = before.manifest.digest.clone(); }
            }
        }
        outcome.artifact = Some(artifact);
        Ok(outcome)
    }
}

/// Cargo's own message names the executable; no cache-layout or mtime fallback.
fn select_artifact(
    stdout: &[u8], request: &BuildRequest, source: &Path, owned: &Path,
    metadata: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut selected = None;
    let mut finished = false;
    for line in stdout.split(|byte| *byte == b'\n') {
        if line.len() > MAX_MESSAGE_BYTES { return Err("compiler message exceeds the finite record bound".into()); }
        if !line.starts_with(b"{") { continue; }
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|error| format!("malformed Cargo JSON evidence: {error}"))?;
        match value.get("reason").and_then(serde_json::Value::as_str) {
            Some("build-finished") => {
                if finished || value.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
                    return Err("Cargo did not report one successful build-finished event".into());
                }
                finished = true;
            }
            Some("compiler-artifact") => {
                if finished { return Err("compiler artifact arrived after build-finished".into()); }
                let target = &value["target"];
                if target["name"].as_str() != Some(request.binary_name.as_str())
                    || target["kind"] != serde_json::json!(["bin"])
                    || value["profile"]["test"].as_bool() != Some(false) { continue; }
                let executable = value["executable"].as_str().filter(|path| !path.is_empty())
                    .ok_or_else(|| "matching compiler artifact has no executable".to_owned())?;
                if !Path::new(executable).is_absolute() { return Err("compiler executable path is not absolute".into()); }
                let canonical_executable = std::fs::canonicalize(executable)
                    .map_err(|error| format!("compiler executable cannot be verified: {error}"))?;
                if !canonical_executable.starts_with(owned) {
                    return Err("compiler executable is outside this build's owned output tree".into());
                }
                let package = value["package_id"].as_str().filter(|id| !id.is_empty());
                let manifest = value["manifest_path"].as_str().map(PathBuf::from)
                    .ok_or_else(|| "compiler artifact has no manifest identity".to_owned())?;
                let manifest = std::fs::canonicalize(manifest)
                    .map_err(|error| format!("compiler manifest cannot be verified: {error}"))?;
                if package.is_none() || !manifest.starts_with(source) {
                    return Err("compiler artifact package is outside the requested source observation".into());
                }
                validate_metadata_target(metadata, &value, &manifest)?;
                if value["filenames"].as_array().is_none_or(|paths|
                    !paths.iter().any(|path| path.as_str() == Some(executable))) {
                    return Err("compiler executable is absent from its artifact filenames".into());
                }
                if selected.replace(value).is_some() { return Err("multiple matching compiler artifacts are ambiguous".into()); }
            }
            _ => {}
        }
    }
    if !finished { return Err("Cargo build-finished evidence is missing".into()); }
    selected.ok_or_else(|| "Cargo did not report the requested binary artifact".into())
}

struct NormalizedArguments {
    build: Vec<String>,
    metadata: Vec<String>,
    target_root: Option<PathBuf>,
}

fn normalized_arguments(arguments: &[String]) -> Result<NormalizedArguments, String> {
    let mut normalized = NormalizedArguments { build: Vec::new(), metadata: Vec::new(), target_root: None };
    let mut manifest_seen = false;
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" || argument.starts_with("-C") {
            return Err("extra arguments cannot change the Cargo working directory or terminate managed arguments".into());
        }
        if ["--message-format", "--artifact-dir", "--out-dir", "--lockfile-path"].iter().any(|name|
            argument.as_str() == *name || argument.starts_with(&format!("{name}="))) {
            return Err("extra arguments cannot override managed compiler evidence, output, or lockfile semantics".into());
        }
        let mut handled = false;
        for name in ["--target-dir", "--manifest-path", "--config"] {
            let prefix = format!("{name}=");
            let value = if argument == name {
                index += 1;
                Some(arguments.get(index).filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| format!("{name} requires an unambiguous value"))?.as_str())
            } else { argument.strip_prefix(&prefix) };
            let Some(value) = value else { continue };
            if value.is_empty() { return Err(format!("{name} cannot be empty")); }
            match name {
                "--target-dir" => {
                    if normalized.target_root.replace(PathBuf::from(value)).is_some() {
                        return Err("multiple --target-dir root preferences are ambiguous".into());
                    }
                }
                _ => {
                    if name == "--manifest-path" {
                        if manifest_seen { return Err("multiple --manifest-path arguments are ambiguous".into()); }
                        manifest_seen = true;
                    }
                    normalized.build.extend([name.to_owned(), value.to_owned()]);
                    normalized.metadata.extend([name.to_owned(), value.to_owned()]);
                }
            }
            handled = true;
            break;
        }
        if !handled {
            normalized.build.push(argument.clone());
            if argument == "--offline" || argument == "--frozen" {
                normalized.metadata.push(argument.clone());
            }
        }
        index += 1;
    }
    Ok(normalized)
}

fn owned_output_layout(
    build_id: EntityId, source: &Path, provider_root: Option<&Path>, environment_root: Option<&Path>,
    argument_root: Option<&Path>, metadata: &serde_json::Value,
) -> Result<BuildOutputLayout, String> {
    let metadata_root = metadata["target_directory"].as_str().map(PathBuf::from)
        .filter(|path| path.is_absolute()).ok_or_else(|| "Cargo did not report an absolute target_directory".to_owned())?;
    let preferred = argument_root.unwrap_or(&metadata_root);
    let preferred = if preferred.is_absolute() { preferred.to_owned() } else { source.join(preferred) };
    std::fs::create_dir_all(&preferred).map_err(|error| format!("target root creation: {error}"))?;
    let preferred = std::fs::canonicalize(preferred).map_err(|error| format!("target root identity: {error}"))?;
    let attempts = preferred.join(".shenko-builds");
    std::fs::create_dir_all(&attempts).map_err(|error| format!("owned output parent: {error}"))?;
    let attempts = std::fs::canonicalize(attempts).map_err(|error| format!("owned output parent identity: {error}"))?;
    if !attempts.starts_with(&preferred) { return Err("owned output parent escaped its canonical target root".into()); }
    let owned = attempts.join(build_id.to_string());
    std::fs::create_dir(&owned).map_err(|error| format!("unique build output creation: {error}"))?;
    let owned = std::fs::canonicalize(owned).map_err(|error| format!("owned output identity: {error}"))?;
    if !owned.starts_with(&attempts) { return Err("owned output escaped its unique parent".into()); }
    Ok(BuildOutputLayout {
        provider_root_preference: provider_root.map(Path::to_owned),
        environment_root_preference: environment_root.map(Path::to_owned),
        argument_root_preference: argument_root.map(Path::to_owned),
        cargo_metadata_root: metadata_root,
        canonical_preferred_root: preferred,
        owned_directory: owned,
    })
}

fn validate_metadata_target(
    metadata: &serde_json::Value, artifact: &serde_json::Value, manifest: &Path,
) -> Result<(), String> {
    let packages = metadata["packages"].as_array().ok_or_else(|| "Cargo metadata packages are missing".to_owned())?;
    let mut matching = packages.iter().filter(|package| package["id"] == artifact["package_id"]);
    let package = matching.next().ok_or_else(|| "compiler package is absent from the original metadata".to_owned())?;
    if matching.next().is_some() { return Err("metadata package identity is ambiguous".into()); }
    let metadata_manifest = package["manifest_path"].as_str().ok_or_else(|| "metadata manifest is missing".to_owned())?;
    if std::fs::canonicalize(metadata_manifest).map_err(|error| error.to_string())? != manifest {
        return Err("compiler manifest differs from the original package metadata".into());
    }
    let declared = package["targets"].as_array().is_some_and(|targets| targets.iter().any(|target|
        target["name"] == artifact["target"]["name"] && target["kind"] == artifact["target"]["kind"]
            && target["src_path"] == artifact["target"]["src_path"]));
    if !declared { return Err("compiler target differs from the original workspace metadata".into()); }
    Ok(())
}

fn probe_limits() -> BuildLimits { BuildLimits { timeout_ms: 15_000, stream_bytes: PROBE_OUTPUT_BYTES } }

async fn observe_source(source: &Path) -> SourceObservation {
    SourceObservation {
        head: observe_command(Path::new("git"), &["rev-parse".into(), "--verify".into(), "HEAD".into()], source, None, &probe_limits()).await,
        status: observe_command(Path::new("git"), &["status".into(), "--porcelain=v1".into(), "-z".into(), "--untracked-files=all".into()], source, None, &probe_limits()).await,
        manifest: observe_file(&source.join("Cargo.toml")),
        lockfile: observe_file(&source.join("Cargo.lock")),
    }
}

fn observe_file(path: &Path) -> FileObservation {
    use std::io::Read;
    let mut observation = FileObservation { path: path.to_owned(), digest: None, bytes: None, error: None };
    let result = (|| -> std::io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take((MAX_MESSAGE_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_MESSAGE_BYTES { return Err(std::io::Error::other("source metadata exceeds observation bound")); }
        Ok(bytes)
    })();
    match result {
        Ok(bytes) => { observation.bytes = Some(bytes.len() as u64); observation.digest = Some(Digest::blake3(&bytes)); }
        Err(error) => observation.error = Some(error.to_string()),
    }
    observation
}

struct BuildChild {
    child: Child,
    tree: OwnedTree,
    settled: bool,
}

impl Drop for BuildChild {
    fn drop(&mut self) {
        if !self.settled {
            let mut notes = CleanupNotes::default();
            self.tree.stop(&mut notes);
            let _ = self.child.start_kill();
            tracing::warn!(cleanup = ?notes.describe(), "build observer dropped before join; no terminal build receipt is claimed");
        }
    }
}

async fn observe_command(program: &Path, args: &[String], cwd: &Path, target_dir: Option<&Path>, limits: &BuildLimits) -> CommandObservation {
    let mut command_line = vec![program.to_string_lossy().into_owned()];
    command_line.extend(args.iter().cloned());
    let mut report = CommandObservation {
        command: command_line, cwd: cwd.to_owned(), explicit_target_dir: target_dir.map(Path::to_owned),
        limits: limits.clone(), started_at: now(), finished_at: String::new(), pid: None,
        exit_code: None, local_process_reaped: false, stdout: CapturedStream::default(),
        stderr: CapturedStream::default(), error: None, cleanup_error: None,
    };
    let Some(deadline) = tokio::time::Instant::now().checked_add(Duration::from_millis(limits.timeout_ms)) else {
        report.error = Some("command deadline is not representable".into());
        report.finished_at = now();
        return report;
    };
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(target_dir) = target_dir { command.env("CARGO_TARGET_DIR", target_dir); }
    OwnedTree::configure(&mut command);
    let tree = match OwnedTree::before_spawn() {
        Ok(tree) => tree,
        Err(error) => { report.error = Some(error.to_string()); report.finished_at = now(); return report; }
    };
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => { report.error = Some(format!("process did not spawn: {error}")); report.finished_at = now(); return report; }
    };
    let mut owner = BuildChild { child, tree, settled: false };
    report.pid = owner.child.id();
    if let Err(error) = owner.tree.adopt(&owner.child) { report.error = Some(error.to_string()); }
    let stdout = owner.child.stdout.take();
    let stderr = owner.child.stderr.take();
    let (Some(mut stdout), Some(mut stderr)) = (stdout, stderr) else {
        report.error = Some("spawned process did not expose its configured output pipes".into());
        report.cleanup_error = owner.tree.retire(&mut owner.child).await.describe();
        report.local_process_reaped = owner.child.try_wait().is_ok_and(|status| status.is_some());
        owner.settled = report.local_process_reaped;
        report.finished_at = now();
        return report;
    };
    let mut out_buffer = [0u8; 8192];
    let mut err_buffer = [0u8; 8192];
    // Keep the leader unreaped until the streams finish. If collection fails,
    // its still-reserved pid anchors the group stop; an unread pipe buffer after
    // reaping would not prove the old group still exists and its id is reusable.
    while report.error.is_none() && !report.local_process_reaped {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                report.error = Some("command or output observation deadline elapsed".into());
            }
            _status = owner.child.wait(), if report.stdout.eof && report.stderr.eof => {
                match _status {
                    Ok(status) => { report.exit_code = status.code(); report.local_process_reaped = true; }
                    Err(error) => report.error = Some(format!("process wait failed: {error}")),
                }
            }
            _count = stdout.read(&mut out_buffer), if !report.stdout.eof => {
                observe_read(&mut report.stdout, _count, &out_buffer, limits.stream_bytes);
                if report.stdout.truncated || report.stdout.error.is_some() { report.error = Some("stdout observation is incomplete".into()); }
            }
            _count = stderr.read(&mut err_buffer), if !report.stderr.eof => {
                observe_read(&mut report.stderr, _count, &err_buffer, limits.stream_bytes);
                if report.stderr.truncated || report.stderr.error.is_some() { report.error = Some("stderr observation is incomplete".into()); }
            }
        }
    }
    if report.error.is_some() {
        report.cleanup_error = owner.tree.retire(&mut owner.child).await.describe();
        match owner.child.try_wait() {
            Ok(Some(status)) => { report.local_process_reaped = true; report.exit_code = status.code(); }
            Ok(None) => report.local_process_reaped = false,
            Err(error) => append_cleanup(&mut report.cleanup_error, format!("process reap observation failed: {error}")),
        }
        let drain = async {
            tokio::join!(drain_output(&mut stdout, &mut report.stdout, limits.stream_bytes),
                drain_output(&mut stderr, &mut report.stderr, limits.stream_bytes));
        };
        if tokio::time::timeout(RETIRE_JOIN, drain).await.is_err() {
            append_cleanup(&mut report.cleanup_error, "output readers did not reach EOF after owned process stop".into());
        }
    }
    owner.settled = report.local_process_reaped && report.stdout.eof && report.stderr.eof;
    report.finished_at = now();
    report
}

fn observe_read(stream: &mut CapturedStream, result: std::io::Result<usize>, buffer: &[u8], limit: usize) {
    match result {
        Ok(0) => stream.eof = true,
        Ok(count) => stream.append(&buffer[..count], limit),
        Err(error) => stream.error = Some(error.to_string()),
    }
}

async fn drain_output(pipe: &mut (impl tokio::io::AsyncRead + Unpin), stream: &mut CapturedStream, limit: usize) {
    let mut buffer = [0u8; 8192];
    while !stream.eof {
        let count = pipe.read(&mut buffer).await;
        let failed = count.is_err();
        observe_read(stream, count, &buffer, limit);
        if failed { break; }
    }
}

fn append_cleanup(existing: &mut Option<String>, error: String) {
    if let Some(previous) = existing { previous.push_str("; "); previous.push_str(&error); }
    else { *existing = Some(error); }
}

pub fn describe_artifact(path: &Path, generation: &RuntimeGeneration) -> Result<CandidateArtifact, BuildError> {
    let bytes = std::fs::read(path)?;
    Ok(CandidateArtifact {
        generation: generation.clone(), path: path.to_owned(), digest: Digest::blake3(&bytes), bytes: bytes.len() as u64,
        source_revision: None, lockfile_digest: None, manifest_digest: None,
        toolchain: "prebuilt".into(), profile: BuildProfile::Release, target: None, built_at: now(),
    })
}

fn now() -> String { chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }

fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_TAIL_BYTES { return text.to_owned(); }
    let boundary = text.len() - OUTPUT_TAIL_BYTES;
    let start = text.char_indices().find(|(index, _)| *index >= boundary).map_or(text.len(), |(index, _)| index);
    text[start..].to_owned()
}

#[cfg(test)]
#[path = "build_tests.rs"]
mod build_tests;
