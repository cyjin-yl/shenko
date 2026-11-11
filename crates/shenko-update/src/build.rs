//! Candidate builds and artifact identity — docs/specs/self-update.md#spec-9-3, docs/specs/self-update.md#spec-35-4.
//!
//! A build provider turns a source tree into a candidate artifact with a
//! content identity. The running device does not have to be the build device:
//! the provider may be a local cargo build, a remote builder or a container
//! toolchain. What the hand-over needs is the identity of the result —
//! source revision, recipe, toolchain, dependency closure and the digest of
//! the artifact itself — not the name of the tool that produced it.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use shenko_types::ids::RuntimeGeneration;
use shenko_types::object::Digest;

/// How much build output is kept as evidence.
const OUTPUT_TAIL_BYTES: usize = 8 * 1024;

/// The build profile of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    fn cargo_dir(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

/// One build request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    /// The source tree that is built.
    pub source_dir: PathBuf,
    /// The generation the artifact belongs to.
    pub generation: RuntimeGeneration,
    pub profile: BuildProfile,
    /// An explicit cargo target triple, when the build is cross-compiled.
    pub target: Option<String>,
    /// Extra cargo arguments, e.g. `--features`.
    pub extra_args: Vec<String>,
    /// The binary name inside the package.
    pub binary_name: String,
}

impl BuildRequest {
    pub fn new(source_dir: impl Into<PathBuf>, generation: RuntimeGeneration) -> Self {
        Self {
            source_dir: source_dir.into(),
            generation,
            profile: BuildProfile::Release,
            target: None,
            extra_args: Vec::new(),
            binary_name: "shenko".to_string(),
        }
    }
}

/// The identity of a built candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateArtifact {
    pub generation: RuntimeGeneration,
    pub path: PathBuf,
    /// The content digest of the artifact itself.
    pub digest: Digest,
    pub bytes: u64,
    /// The source revision the build came from, when the tree is a checkout.
    pub source_revision: Option<String>,
    /// The digest of the locked dependency closure.
    pub lockfile_digest: Option<Digest>,
    /// The digest of the workspace manifest.
    pub manifest_digest: Option<Digest>,
    /// The toolchain that produced the artifact.
    pub toolchain: String,
    pub profile: BuildProfile,
    pub target: Option<String>,
    pub built_at: String,
}

/// The recorded outcome of a build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutcome {
    pub artifact: Option<CandidateArtifact>,
    pub exit_code: i32,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub command: Vec<String>,
}

/// Errors of the build provider.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("the build failed with exit code {exit_code}: {stderr}")]
    Failed {
        exit_code: i32,
        stderr: String,
    },
    #[error("the build produced no artifact at {0}")]
    MissingArtifact(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A source of candidate artifacts.
#[async_trait::async_trait]
pub trait BuildProvider: Send + Sync {
    /// Build the candidate and return its identity. A failed build is an
    /// error, never an artifact without a digest.
    async fn build(&self, request: &BuildRequest) -> Result<BuildOutcome, BuildError>;
}

/// The local cargo builder.
#[derive(Debug, Clone)]
pub struct CargoBuildProvider {
    /// The cargo executable. It may live outside the default PATH.
    pub cargo: PathBuf,
    /// An explicit target directory, e.g. a persistent build root.
    pub target_dir: Option<PathBuf>,
}

impl Default for CargoBuildProvider {
    fn default() -> Self {
        Self {
            cargo: PathBuf::from("cargo"),
            target_dir: None,
        }
    }
}

impl CargoBuildProvider {
    pub fn new(cargo: impl Into<PathBuf>) -> Self {
        Self {
            cargo: cargo.into(),
            target_dir: None,
        }
    }

    /// The path the artifact of this request is expected at.
    pub fn artifact_path(&self, request: &BuildRequest) -> PathBuf {
        let mut path = request.source_dir.join("target");
        if let Some(target) = &request.target {
            path = path.join(target);
        }
        path.join(request.profile.cargo_dir())
            .join(&request.binary_name)
    }
}

#[async_trait::async_trait]
impl BuildProvider for CargoBuildProvider {
    async fn build(&self, request: &BuildRequest) -> Result<BuildOutcome, BuildError> {
        let toolchain = self.toolchain().await.unwrap_or_else(|_| "unknown".to_string());
        let mut args: Vec<String> = vec!["build".to_string()];
        match request.profile {
            BuildProfile::Release => args.push("--release".to_string()),
            BuildProfile::Debug => {}
        }
        args.push("--locked".to_string());
        args.push("--bin".to_string());
        args.push(request.binary_name.clone());
        if let Some(target) = &request.target {
            args.push("--target".to_string());
            args.push(target.clone());
        }
        args.extend(request.extra_args.iter().cloned());

        let mut command = Command::new(&self.cargo);
        command
            .args(&args)
            .current_dir(&request.source_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(target_dir) = &self.target_dir {
            command.env("CARGO_TARGET_DIR", target_dir);
        }

        let output = command.output().await?;
        let stdout_tail = tail(&String::from_utf8_lossy(&output.stdout));
        let stderr_tail = tail(&String::from_utf8_lossy(&output.stderr));
        let exit_code = output.status.code().unwrap_or(-1);
        let mut command_line = vec![self.cargo.to_string_lossy().to_string()];
        command_line.extend(args.iter().cloned());

        if !output.status.success() {
            return Err(BuildError::Failed {
                exit_code,
                stderr: stderr_tail,
            });
        }

        let artifact_path = self.artifact_path(request);
        if !artifact_path.is_file() {
            return Err(BuildError::MissingArtifact(artifact_path));
        }
        let artifact = describe_artifact(&artifact_path, &request.generation)?;
        Ok(BuildOutcome {
            artifact: Some(CandidateArtifact {
                toolchain,
                profile: request.profile,
                target: request.target.clone(),
                ..artifact
            }),
            exit_code,
            stdout_tail,
            stderr_tail,
            command: command_line,
        })
    }
}

impl CargoBuildProvider {
    /// The toolchain identity of the builder.
    pub async fn toolchain(&self) -> Result<String, BuildError> {
        let output = Command::new(&self.cargo)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Describe an artifact that was built elsewhere.
///
/// The digest is the artifact's content identity. A file that cannot be read
/// is an error; an artifact without a digest never enters a hand-over.
pub fn describe_artifact(
    path: &Path,
    generation: &RuntimeGeneration,
) -> Result<CandidateArtifact, BuildError> {
    let bytes = std::fs::read(path)?;
    let digest = Digest::blake3(&bytes);
    Ok(CandidateArtifact {
        generation: generation.clone(),
        path: path.to_path_buf(),
        bytes: bytes.len() as u64,
        digest,
        source_revision: None,
        lockfile_digest: None,
        manifest_digest: None,
        toolchain: "prebuilt".to_string(),
        profile: BuildProfile::Release,
        target: None,
        built_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// The locked dependency closure of a source tree.
pub fn lockfile_digest(source_dir: &Path) -> Option<Digest> {
    let bytes = std::fs::read(source_dir.join("Cargo.lock")).ok()?;
    Some(Digest::blake3(&bytes))
}

/// The digest of the workspace manifest.
pub fn manifest_digest(source_dir: &Path) -> Option<Digest> {
    let bytes = std::fs::read(source_dir.join("Cargo.toml")).ok()?;
    Some(Digest::blake3(&bytes))
}

/// The source revision of a checkout, when the tree is a git worktree.
pub async fn source_revision(source_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", &source_dir.to_string_lossy(), "rev-parse", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if revision.is_empty() {
        None
    } else {
        Some(revision)
    }
}

fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text.to_string();
    }
    let start = text.len() - OUTPUT_TAIL_BYTES;
    let start = text
        .char_indices()
        .find(|(index, _)| *index >= start)
        .map(|(index, _)| index)
        .unwrap_or(start);
    text[start..].to_string()
}
