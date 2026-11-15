//! Optional host bridge to an independently installed Obelisk CLI.
//!
//! This module does not own archive truth or a second installation registry.
//! Source paths are node-local locators. Callers archive complete source bytes
//! separately and record these observations through their normal task receipts.
//! Obelisk's JS runtime is not a security boundary: custom scripts execute with
//! the configured host account's authority, even though its SQL helper is read-only.
use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const QUERY_TEMPLATE: &str = include_str!("obelisk_query.js");
const UPSTREAM_MANIFEST: &str = include_str!("obelisk_upstream_manifest.json");
const QUERY_MAX_BYTES: usize = 128 * 1024;
// Explicit empty roots disable implicit discovery in the audited CLI. Future
// versions must be checked before updating this compatibility declaration.
const KNOWN_PROVIDERS: &[&str] = &[
    "claude",
    "codex",
    "codexhost-antigravity",
    "copilot",
    "deepseek",
    "hermes",
    "kimi",
    "kiro",
    "omp",
    "opencode",
    "pi",
    "zcode",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObeliskProvider {
    Codex,
    Omp,
    Pi,
}

impl ObeliskProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Omp => "omp",
            Self::Pi => "pi",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObeliskSourceRoot {
    pub provider: ObeliskProvider,
    /// Stable configured label, independent of this node's filesystem location.
    pub root_label: String,
    /// Codex: its .codex root; OMP/Pi: their sessions directory.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObeliskLimits {
    pub timeout_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub drain_timeout_ms: u64,
}
impl Default for ObeliskLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            stdout_bytes: 1024 * 1024,
            stderr_bytes: 64 * 1024,
            drain_timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObeliskConfig {
    pub executable: PathBuf,
    /// Checkout of the audited independent upstream program. Source bytes,
    /// rather than its ambiguous --version string, bind this integration.
    pub source_checkout: PathBuf,
    /// Explicit executable arguments, e.g. a separately installed Node CLI path.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Dedicated disposable index directory. Existing different settings are
    /// rejected instead of overwriting another Obelisk installation.
    pub index_home: PathBuf,
    /// Logical origin identity; it must survive moving or replicating files.
    pub source_instance: String,
    pub sources: Vec<ObeliskSourceRoot>,
    #[serde(default)]
    pub limits: ObeliskLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObeliskMessageCursor {
    pub timestamp: Option<String>,
    pub uuid: String,
}

fn default_limit() -> usize {
    20
}
fn default_raw_limit() -> usize {
    16_384
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObeliskQuery {
    Script {
        script: String,
    },
    Sessions {
        #[serde(default)]
        provider: Option<ObeliskProvider>,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Search {
        query: String,
        #[serde(default)]
        provider: Option<ObeliskProvider>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        project_path: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        before: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        include_inactive: bool,
        #[serde(default = "default_true")]
        literal_fallback: bool,
    },
    Messages {
        session_id: String,
        #[serde(default)]
        cursor: Option<ObeliskMessageCursor>,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        include_inactive: bool,
    },
    RawPage {
        message_uuid: String,
        #[serde(default)]
        offset_utf16: usize,
        #[serde(default = "default_raw_limit")]
        limit_utf16: usize,
        #[serde(default)]
        include_inactive: bool,
    },
    Manifest {
        #[serde(default)]
        after_path: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObeliskSourceRecord {
    pub id: String,
    pub role: Option<String>,
    pub timestamp: Option<String>,
    pub parent_id: Option<String>,
    pub agent_id: Option<String>,
    pub content_type: Option<String>,
    pub visibility: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObeliskSourceAvailability {
    Available,
    Missing,
    Unreadable,
    NotRegularFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObeliskSourceManifest {
    pub source_instance: String,
    pub provider: ObeliskProvider,
    pub source_key: String,
    pub identity_basis: String,
    pub session_id: Option<String>,
    pub local_path: PathBuf,
    pub availability: ObeliskSourceAvailability,
    pub error: Option<String>,
    pub observed_size: Option<u64>,
    pub observed_modified_ms: Option<u64>,
    pub indexed_mtime_ms: Option<f64>,
    /// Unknown: the public generic index schema does not expose this value.
    pub indexed_size: Option<u64>,
    pub index_cursor: Option<String>,
    /// The private cursor can contain a large Bloom filter. It stays in the
    /// disposable index, not in each model-facing observation.
    pub index_cursor_bytes: Option<u64>,
    /// false on replay-required; otherwise unknown, never guessed from mtime.
    pub indexed_complete: Option<bool>,
    pub records: Vec<ObeliskSourceRecord>,
    pub records_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObeliskRunStatus {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    OutputLimit,
    InvalidResponse,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObeliskProcessObservation {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub index_home: PathBuf,
    pub query_digest: String,
    /// Preparing the dedicated disposable index and query file can write local
    /// files before an external process starts. Neither flag implies a spawn.
    pub index_preparation_started: bool,
    pub index_preparation_complete: bool,
    pub started: bool,
    pub pid: Option<u32>,
    pub termination_requested: bool,
    pub exit_code: Option<i32>,
    pub reaped: bool,
    pub readers_joined: bool,
    pub readers_drained: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Failed/undecodable output retains the exact captured prefix. The
    /// truncation flag says whether the output budget omitted further bytes.
    pub stdout_base64: Option<String>,
    pub stderr: String,
    /// UTF-8 stderr is already lossless above; retain bytes when it is not.
    pub stderr_base64: Option<String>,
    pub error: Option<String>,
    /// This is process-group signalling, not a sandbox or proof that a child
    /// that deliberately escaped its process group has stopped.
    pub termination_scope: String,
    pub custom_script_host_authority: bool,
    pub installation: ObeliskInstallationProof,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObeliskInstallationProof {
    pub repository: String,
    pub revision: String,
    pub source_checkout: PathBuf,
    pub manifest_digest: String,
    pub verified_files: usize,
    pub contract: String,
}

#[derive(Deserialize)]
struct UpstreamManifest {
    repository: String,
    revision: String,
    contract: String,
    files: Vec<UpstreamFile>,
}
#[derive(Deserialize)]
struct UpstreamFile {
    path: String,
    bytes: u64,
    blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObeliskObservation {
    pub schema_version: u32,
    pub status: ObeliskRunStatus,
    pub source_instance: String,
    pub data: Option<Value>,
    pub source_manifests: Vec<ObeliskSourceManifest>,
    pub manifest_next_path: Option<String>,
    pub coverage: Value,
    pub process: ObeliskProcessObservation,
}

#[derive(Debug, Clone)]
pub struct ObeliskRuntime {
    config: ObeliskConfig,
}

impl ObeliskConfig {
    pub fn validate(&self) -> Result<()> {
        let config = self;
        ensure!(
            config.executable.is_absolute(),
            "Obelisk executable must be an explicit absolute path"
        );
        ensure!(
            config.source_checkout.is_absolute(),
            "source_checkout must be absolute"
        );
        let entrypoint = config.source_checkout.join("packages/cli/src/obelisk.ts");
        ensure!(
            config.arguments == [entrypoint.to_string_lossy().to_string()],
            "Obelisk arguments must select the audited source checkout's packages/cli/src/obelisk.ts; an unverified npm CLI may ignore OBELISK_HOME and scan unintended history"
        );
        ensure!(
            config.index_home.is_absolute(),
            "Obelisk index_home must be absolute"
        );
        ensure!(
            !config.source_instance.trim().is_empty() && config.source_instance.len() <= 512,
            "source_instance must be a stable nonempty identity (at most 512 bytes)"
        );
        ensure!(
            !config.sources.is_empty(),
            "at least one source root is required"
        );
        let mut providers = BTreeSet::new();
        let mut labels = BTreeSet::new();
        for source in &config.sources {
            ensure!(
                source.path.is_absolute() && source.path.to_str().is_some(),
                "source roots must be absolute UTF-8 paths"
            );
            ensure!(
                !source.root_label.trim().is_empty()
                    && !source.root_label.contains(['/', '\\', '\0']),
                "root_label must be a nonempty logical label without path separators"
            );
            ensure!(
                providers.insert(source.provider),
                "the upstream CLI accepts one root per provider in one index"
            );
            ensure!(
                labels.insert(source.root_label.clone()),
                "root labels must be unique"
            );
            ensure!(
                !source
                    .path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
                "source roots must not contain parent-directory components"
            );
        }
        for (index, left) in config.sources.iter().enumerate() {
            for right in &config.sources[index + 1..] {
                ensure!(
                    !left.path.starts_with(&right.path) && !right.path.starts_with(&left.path),
                    "overlapping provider roots make source provenance ambiguous"
                );
            }
        }
        ensure!(
            config.limits.timeout_ms > 0 && config.limits.drain_timeout_ms > 0,
            "process deadlines must be positive"
        );
        ensure!(
            config.limits.stdout_bytes > 0 && config.limits.stderr_bytes > 0,
            "output budgets must be positive"
        );
        Ok(())
    }
}

impl ObeliskRuntime {
    pub fn new(config: ObeliskConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &ObeliskConfig {
        &self.config
    }

    /// No process or index refresh occurs during preflight. Published 0.3.0
    /// and main's 0.3.0 have different storage semantics, so a version string
    /// cannot prove that configured roots and OBELISK_HOME will be honoured.
    pub fn verify_installation(&self) -> Result<ObeliskInstallationProof> {
        use std::io::Read;
        let manifest: UpstreamManifest = serde_json::from_str(UPSTREAM_MANIFEST)?;
        for expected in &manifest.files {
            let path = self.config.source_checkout.join(&expected.path);
            let file = std::fs::File::open(&path).with_context(|| {
                format!("Obelisk source preflight could not read {}", path.display())
            })?;
            ensure!(
                file.metadata()?.is_file(),
                "Obelisk source is not a regular file: {}",
                path.display()
            );
            let mut bytes = Vec::with_capacity(expected.bytes as usize);
            file.take(expected.bytes + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 == expected.bytes
                    && blake3::hash(&bytes).to_hex().as_str() == expected.blake3,
                "Obelisk source preflight mismatch: {}; expected upstream {}. No query was started",
                expected.path,
                manifest.revision
            );
        }
        Ok(ObeliskInstallationProof {
            repository: manifest.repository,
            revision: manifest.revision,
            source_checkout: self.config.source_checkout.clone(),
            manifest_digest: blake3::hash(UPSTREAM_MANIFEST.as_bytes())
                .to_hex()
                .to_string(),
            verified_files: manifest.files.len(),
            contract: manifest.contract,
        })
    }

    fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config.index_home)?;
        let mut roots = serde_json::Map::new();
        for provider in KNOWN_PROVIDERS {
            roots.insert((*provider).into(), json!(""));
        }
        for source in &self.config.sources {
            roots.insert(source.provider.as_str().into(), json!(source.path));
        }
        let settings = json!({"providerRoots": roots, "copilotEditions": {"stable": false, "insiders": false}});
        let path = self.config.index_home.join("settings.json");
        // Publish a complete settings file without replacing an existing one.
        // Concurrent first queries cannot observe a half-written JSON file.
        let staged = QueryFile::create(&self.config.index_home, &serde_json::to_vec(&settings)?)?;
        match std::fs::hard_link(&staged.0, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let current: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                ensure!(
                    current == settings,
                    "Obelisk index_home already has different settings; choose a dedicated index home instead of overwriting it"
                );
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn script(&self, request: &ObeliskQuery) -> Result<String> {
        match request {
            ObeliskQuery::Script { script } => {
                ensure!(
                    !script.trim().is_empty() && script.len() <= QUERY_MAX_BYTES,
                    "custom query must contain 1..=131072 bytes"
                );
                // No eval, shell quoting, or transcript interpolation. The
                // caller explicitly chose executable code rather than data.
                Ok(format!(
                    "const data=await(async()=>{{\n{script}\n}})();\nreturn {{schema_version:1,data,index_observation:{{refresh_complete:null,original_archive_complete:false,source_text_may_be_truncated:true}}}};\n"
                ))
            }
            _ => {
                let value = serde_json::to_value(request)?;
                if let Some(limit) = value.get("limit").and_then(Value::as_u64) {
                    ensure!((1..=100).contains(&limit), "query limit must be 1..=100");
                }
                if let ObeliskQuery::RawPage {
                    offset_utf16,
                    limit_utf16,
                    ..
                } = request
                {
                    ensure!(
                        (1..=65_536).contains(limit_utf16) && *offset_utf16 <= (1_usize << 52),
                        "raw page exceeds its range budget"
                    );
                }
                if let ObeliskQuery::Search { query, .. } = request {
                    ensure!(
                        !query.is_empty() && query.len() <= 4096,
                        "search query must contain 1..=4096 bytes"
                    );
                }
                let sources: Vec<Value> = self
                    .config
                    .sources
                    .iter()
                    .map(|source| {
                        let path = source.path.to_string_lossy().to_string();
                        let prefix = if path.ends_with(std::path::MAIN_SEPARATOR) {
                            path.clone()
                        } else {
                            format!("{path}{}", std::path::MAIN_SEPARATOR)
                        };
                        json!({"provider": source.provider, "path": path, "prefix": prefix})
                    })
                    .collect();
                Ok(format!(
                    "const request={};\nconst binding={};\n{}",
                    value,
                    json!({"sources":sources}),
                    QUERY_TEMPLATE
                ))
            }
        }
    }

    /// The caller must await cancellation to completion before tearing down the
    /// installation. Dropping this future only invokes kill_on_drop; it cannot
    /// itself be evidence that all process readers were joined.
    pub async fn execute(
        &self,
        request: ObeliskQuery,
        cancellation: CancellationToken,
    ) -> Result<ObeliskObservation> {
        let script = self.script(&request)?;
        let installation = self.verify_installation()?;
        let mut observation = ObeliskObservation {
            schema_version: 1,
            status: ObeliskRunStatus::Unavailable,
            source_instance: self.config.source_instance.clone(),
            data: None,
            source_manifests: vec![],
            manifest_next_path: None,
            coverage: json!({"configured_sources": self.config.sources, "refresh_complete":null,
                "original_archive_complete":false, "source_text_may_be_truncated":true,
                "scope":"node_local_snapshot", "distributed_frontier":null}),
            process: ObeliskProcessObservation {
                executable: self.config.executable.clone(),
                arguments: self.config.arguments.clone(),
                index_home: self.config.index_home.clone(),
                query_digest: blake3::hash(script.as_bytes()).to_hex().to_string(),
                index_preparation_started: false,
                index_preparation_complete: false,
                started: false,
                pid: None,
                termination_requested: false,
                exit_code: None,
                reaped: false,
                readers_joined: false,
                readers_drained: false,
                stdout_truncated: false,
                stderr_truncated: false,
                stdout_base64: None,
                stderr: String::new(),
                stderr_base64: None,
                error: None,
                termination_scope: if cfg!(any(
                    target_os = "linux",
                    target_os = "android",
                    target_vendor = "apple"
                )) {
                    "process_group"
                } else {
                    "direct_child"
                }
                .into(),
                custom_script_host_authority: matches!(request, ObeliskQuery::Script { .. }),
                installation,
            },
        };
        let mut source_roots = Vec::new();
        for source in &self.config.sources {
            let (status, error) = match tokio::fs::metadata(&source.path).await {
                Ok(metadata) if metadata.is_dir() => ("available", None),
                Ok(_) => (
                    "not_directory",
                    Some("provider root is not a directory".to_owned()),
                ),
                Err(error) => (
                    if error.kind() == std::io::ErrorKind::NotFound {
                        "missing"
                    } else {
                        "unreadable"
                    },
                    Some(error.to_string()),
                ),
            };
            source_roots.push(json!({"provider":source.provider,"root_label":source.root_label,"path":source.path,"status":status,"error":error}));
        }
        observation.coverage["source_roots"] = json!(source_roots);
        if cancellation.is_cancelled() {
            observation.status = ObeliskRunStatus::Cancelled;
            return Ok(observation);
        }
        // Everything that may write is represented in the observation. Err is
        // reserved for the parameter and source-byte checks above, before any
        // index preparation or process dispatch.
        observation.process.index_preparation_started = true;
        if let Err(error) = self.prepare() {
            observation.process.error = Some(format!("index preparation failed: {error:#}"));
            return Ok(observation);
        }
        let query_file = match QueryFile::create(&self.config.index_home, script.as_bytes()) {
            Ok(file) => file,
            Err(error) => {
                observation.process.error =
                    Some(format!("query file preparation failed: {error:#}"));
                return Ok(observation);
            }
        };
        observation.process.index_preparation_complete = true;
        let mut arguments = self.config.arguments.clone();
        arguments.extend([
            "--query".into(),
            query_file.0.to_string_lossy().into_owned(),
        ]);
        observation.process.arguments = arguments.clone();
        if cancellation.is_cancelled() {
            observation.status = ObeliskRunStatus::Cancelled;
            return Ok(observation);
        }
        let mut command = Command::new(&self.config.executable);
        command
            .args(&arguments)
            .env("OBELISK_HOME", &self.config.index_home)
            .env_remove("NODE_OPTIONS")
            .env_remove("NODE_PATH")
            .current_dir(&self.config.index_home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                observation.process.error = Some(error.to_string());
                return Ok(observation);
            }
        };
        let pid = child.id();
        observation.process.started = true;
        observation.process.pid = pid;
        let exceeded = CancellationToken::new();
        let pipe_missing = child.stdout.is_none() || child.stderr.is_none();
        let stdout = tokio::spawn(capture(
            child.stdout.take(),
            self.config.limits.stdout_bytes,
            exceeded.clone(),
        ));
        let stderr = tokio::spawn(capture(
            child.stderr.take(),
            self.config.limits.stderr_bytes,
            exceeded.clone(),
        ));
        let completed = if pipe_missing {
            observation.status = ObeliskRunStatus::Failed;
            observation.process.error = Some("spawned process pipe is unavailable".into());
            None
        } else {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => { observation.status = ObeliskRunStatus::Cancelled; None }
                _ = exceeded.cancelled() => { observation.status = ObeliskRunStatus::OutputLimit; None }
                _ = tokio::time::sleep(Duration::from_millis(self.config.limits.timeout_ms)) => { observation.status = ObeliskRunStatus::TimedOut; None }
                _result = child.wait() => Some(_result),
            }
        };
        if let Some(result) = completed {
            match result {
                Ok(status) => {
                    observation.process.reaped = true;
                    observation.process.exit_code = status.code();
                    observation.status = if status.success() {
                        ObeliskRunStatus::Succeeded
                    } else {
                        ObeliskRunStatus::Failed
                    };
                }
                Err(error) => {
                    observation.status = ObeliskRunStatus::Failed;
                    observation.process.error = Some(error.to_string());
                }
            }
        } else {
            observation.process.termination_requested = true;
            stop_group(pid);
            let _ = child.start_kill();
            match tokio::time::timeout(
                Duration::from_millis(self.config.limits.drain_timeout_ms),
                child.wait(),
            )
            .await
            {
                Ok(Ok(status)) => {
                    observation.process.reaped = true;
                    observation.process.exit_code = status.code();
                }
                Ok(Err(error)) => {
                    observation.process.error = Some(format!("process wait failed: {error}"))
                }
                Err(_) => {
                    observation.process.error =
                        Some("process termination was requested; reap deadline elapsed".into())
                }
            }
        }
        let grace = Duration::from_millis(self.config.limits.drain_timeout_ms);
        let (stdout, stderr) =
            tokio::join!(join_capture(stdout, grace), join_capture(stderr, grace));
        observation.process.readers_joined = stdout.joined && stderr.joined;
        observation.process.readers_drained = stdout.drained && stderr.drained;
        observation.process.stdout_truncated = stdout.truncated;
        observation.process.stderr_truncated = stderr.truncated;
        observation.process.stderr = String::from_utf8_lossy(&stderr.bytes).into_owned();
        if std::str::from_utf8(&stderr.bytes).is_err() {
            observation.process.stderr_base64 =
                Some(base64::engine::general_purpose::STANDARD.encode(&stderr.bytes));
        }
        if (stdout.truncated || stderr.truncated)
            && observation.status == ObeliskRunStatus::Succeeded
        {
            observation.status = ObeliskRunStatus::OutputLimit;
        }
        if let Some(error) = stdout.error.or(stderr.error) {
            observation.process.error = Some(error);
            if observation.status == ObeliskRunStatus::Succeeded {
                observation.status = ObeliskRunStatus::Failed;
            }
        }
        if !observation.process.readers_drained {
            observation.process.termination_requested = true;
            stop_group(pid);
            if observation.status == ObeliskRunStatus::Succeeded {
                observation.status = ObeliskRunStatus::Failed;
            }
            observation.process.error =
                Some("reader drain deadline elapsed; readers were aborted and joined".into());
        }
        if observation.status == ObeliskRunStatus::Succeeded {
            match serde_json::from_slice::<Value>(&stdout.bytes) {
                Ok(envelope)
                    if envelope.get("schema_version") == Some(&json!(1))
                        && envelope.get("data").is_some() =>
                {
                    observation.data = envelope.get("data").cloned();
                    observation.coverage["index_observation"] = envelope
                        .get("index_observation")
                        .cloned()
                        .unwrap_or(Value::Null);
                    observation.coverage["index_warnings"] =
                        json!(!observation.process.stderr.is_empty());
                    observation.manifest_next_path = envelope
                        .get("manifest_next_path")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if let Some(rows) = envelope.get("manifest_rows").and_then(Value::as_array) {
                        for row in rows {
                            match self.source_manifest(row).await {
                                Ok(manifest) => observation.source_manifests.push(manifest),
                                Err(error) => {
                                    observation.status = ObeliskRunStatus::InvalidResponse;
                                    observation.process.error =
                                        Some(format!("invalid source manifest: {error}"));
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(_) => {
                    observation.status = ObeliskRunStatus::InvalidResponse;
                    observation.process.error =
                        Some("upstream output is not the expected query envelope".into());
                }
                Err(error) => {
                    observation.status = ObeliskRunStatus::InvalidResponse;
                    observation.process.error = Some(format!(
                        "upstream stdout is not a single JSON response: {error}"
                    ));
                }
            }
        } else if let Ok(error) = serde_json::from_slice::<Value>(&stdout.bytes) {
            observation.data = Some(error);
        }
        if observation.status != ObeliskRunStatus::Succeeded && !stdout.bytes.is_empty() {
            observation.process.stdout_base64 =
                Some(base64::engine::general_purpose::STANDARD.encode(&stdout.bytes));
        }
        Ok(observation)
    }

    async fn source_manifest(&self, row: &Value) -> Result<ObeliskSourceManifest> {
        let path = PathBuf::from(
            row.get("local_path")
                .and_then(Value::as_str)
                .context("manifest source path missing")?,
        );
        let source = self
            .config
            .sources
            .iter()
            .find(|source| path.starts_with(&source.path))
            .context("manifest source is outside configured roots")?;
        let session_id = row
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (source_key, identity_basis) = if let Some(id) = &session_id {
            (format!("session:{id}"), "native_session")
        } else {
            let relative = path
                .strip_prefix(&source.path)?
                .to_str()
                .context("source relative path is not UTF-8")?
                .replace('\\', "/");
            (
                format!("root:{}/{relative}", source.root_label),
                "configured_root_relative_path",
            )
        };
        let metadata = async {
            let metadata = tokio::fs::metadata(&path).await?;
            if metadata.is_file() {
                // A directory entry can be visible while its contents are not
                // readable. Probe the original without changing any bytes;
                // this is availability at observation time, not a snapshot.
                tokio::fs::File::open(&path).await?.metadata().await
            } else {
                Ok(metadata)
            }
        }
        .await;
        let (availability, size, modified, error) = match metadata {
            Ok(metadata) if metadata.is_file() => (
                ObeliskSourceAvailability::Available,
                Some(metadata.len()),
                metadata.modified().ok().and_then(epoch_ms),
                None,
            ),
            Ok(_) => (
                ObeliskSourceAvailability::NotRegularFile,
                None,
                None,
                Some("source locator is not a regular file".into()),
            ),
            Err(error) => (
                if error.kind() == std::io::ErrorKind::NotFound {
                    ObeliskSourceAvailability::Missing
                } else {
                    ObeliskSourceAvailability::Unreadable
                },
                None,
                None,
                Some(error.to_string()),
            ),
        };
        Ok(ObeliskSourceManifest {
            source_instance: self.config.source_instance.clone(),
            provider: source.provider,
            source_key,
            identity_basis: identity_basis.into(),
            session_id,
            local_path: path,
            availability,
            error,
            observed_size: size,
            observed_modified_ms: modified,
            indexed_mtime_ms: row.get("indexed_mtime_ms").and_then(Value::as_f64),
            indexed_size: None,
            index_cursor: row
                .get("index_cursor")
                .and_then(Value::as_str)
                .map(str::to_owned),
            index_cursor_bytes: row.get("index_cursor_bytes").and_then(Value::as_u64),
            indexed_complete: (row.get("replay_required").and_then(Value::as_i64) == Some(1))
                .then_some(false),
            records: serde_json::from_value(row.get("records").cloned().unwrap_or(json!([])))?,
            records_truncated: row
                .get("records_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

fn epoch_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

struct QueryFile(PathBuf);
impl QueryFile {
    fn create(directory: &Path, bytes: &[u8]) -> Result<Self> {
        use std::io::Write;
        let path = directory.join(format!("shenko-query-{}.js", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let guard = Self(path);
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(guard)
    }
}
impl Drop for QueryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
    joined: bool,
    drained: bool,
    error: Option<String>,
}
async fn capture(
    reader: Option<impl AsyncRead + Unpin>,
    limit: usize,
    exceeded: CancellationToken,
) -> Capture {
    let Some(mut reader) = reader else {
        return Capture {
            error: Some("spawned process pipe is unavailable".into()),
            ..Default::default()
        };
    };
    let mut capture = Capture::default();
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                capture.drained = true;
                return capture;
            }
            Ok(count) => {
                let remaining = limit.saturating_sub(capture.bytes.len());
                capture
                    .bytes
                    .extend_from_slice(&buffer[..count.min(remaining)]);
                if count > remaining {
                    capture.truncated = true;
                    exceeded.cancel();
                }
            }
            Err(error) => {
                capture.error = Some(format!("process stream read failed: {error}"));
                return capture;
            }
        }
    }
}
async fn join_capture(mut handle: JoinHandle<Capture>, grace: Duration) -> Capture {
    match tokio::time::timeout(grace, &mut handle).await {
        Ok(Ok(mut capture)) => {
            capture.joined = true;
            capture
        }
        Ok(Err(error)) => Capture {
            joined: true,
            error: Some(format!("reader task failed: {error}")),
            ..Default::default()
        },
        Err(_) => {
            handle.abort();
            let _ = handle.await;
            Capture {
                joined: true,
                truncated: true,
                error: Some("reader drain timed out".into()),
                ..Default::default()
            }
        }
    }
}

fn stop_group(pid: Option<u32>) {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    if let Some(pid) = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    let _ = pid;
}

#[cfg(test)]
mod tests;
