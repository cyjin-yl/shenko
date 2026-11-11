//! The harness of the self-learning proof consumer: the **actual default CLI**
//! as the process under test, and this consumer's only ways to reach it.
//!
//! The consumer never builds a broker, a subject loop or a native child of its
//! own. It starts the real `shenko start` binary, submits one user message
//! through the product's own web-input surface, watches the product's own task
//! read model, stops the process through its real SIGINT shutdown, and only
//! then reads the installation's own durable facts. Nothing here fabricates an
//! attempt, a measurement or a verdict.
//!
//! Source binding (`--binary`/`--repo`) is recorded as declared evidence, never
//! as build provenance: a sha256 of the binary and of each relevant source file
//! plus the repository HEAD. A file that is absent is recorded as absent.

use anyhow::{Context as _, Result, bail, ensure};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The source files this consumer's claims depend on. They are hashed as the
/// current source binding; a missing file stays visible as `absent` instead of
/// being skipped (an unmerged child must not look like a merged one).
pub const BOUND_SOURCES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "bin/shenko/src/lib.rs",
    "bin/shenko/src/main.rs",
    "crates/shenko-exec-host/Cargo.toml",
    "crates/shenko-exec-host/src/cognition_task.rs",
    "crates/shenko-exec-host/src/cognition_trial.rs",
    "crates/shenko-exec-host/src/cognition_trial/preparation.rs",
    "crates/shenko-exec-host/src/cognition_trial/judge_receipt.rs",
    "crates/shenko-exec-host/src/cognition_query.rs",
    "crates/shenko-exec-host/src/cognition_judge.rs",
    "crates/shenko-awareness-core/src/cognition.rs",
    "crates/shenko-node/src/units.rs",
    "crates/shenko-ui/src/input.rs",
    "crates/shenko-ui/src/ws.rs",
    "docs/specs/cognition-units.md",
];

/// The normative reading this consumer applies. These are not build inputs: they
/// are recorded so the receipt names the contract it was written against
/// (PR180 merge `2cb17172cfd65291997df5953f54d0ab5abe4c38`).
pub const CONTRACT_SOURCES: &[&str] = &[
    "docs/contracts.json",
    "docs/specs/cognition-units.md",
    "docs/specs/verification.md",
    "docs/decisions/0002-cognitive-atoms-2026-11-05.md",
    "docs/decisions/0003-spec-governance-2026-11-06.md",
];

/// Everything the consumer was told on its own command line.
#[derive(Debug, Clone)]
pub struct Options {
    /// The built default CLI under test.
    pub binary: PathBuf,
    /// The source checkout the binary is claimed to come from.
    pub repo: PathBuf,
    /// Fresh run directory for this invocation.
    pub root: PathBuf,
    /// How long the subject may work on the message before the consumer stops it.
    pub authoring: Duration,
    /// How long one CLI start may take before it is considered failed.
    pub startup: Duration,
    /// How long a graceful SIGINT shutdown may take before SIGKILL is reported.
    pub shutdown: Duration,
    /// Arguments the test runner did not consume, kept verbatim in the receipt.
    pub other_args: Vec<String>,
}

impl Options {
    /// Parse `--binary`, `--repo`, `--root`, `--authoring-seconds`,
    /// `--startup-seconds`, `--shutdown-seconds`.
    ///
    /// Only the default test-binary arguments reach this parser; anything else
    /// is kept in `other_args` so the receipt states what it ran with.
    pub fn from_args() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut binary = None;
        let mut repo = None;
        let mut root = None;
        let mut authoring = 1_800u64;
        let mut startup = 300u64;
        let mut shutdown = 120u64;
        let mut other = Vec::new();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--binary" => binary = args.next().map(PathBuf::from),
                "--repo" => repo = args.next().map(PathBuf::from),
                "--root" => root = args.next().map(PathBuf::from),
                "--authoring-seconds" => {
                    authoring = args
                        .next()
                        .context("--authoring-seconds needs a value")?
                        .parse()
                        .context("--authoring-seconds is not a number")?
                }
                "--startup-seconds" => {
                    startup = args
                        .next()
                        .context("--startup-seconds needs a value")?
                        .parse()
                        .context("--startup-seconds is not a number")?
                }
                "--shutdown-seconds" => {
                    shutdown = args
                        .next()
                        .context("--shutdown-seconds needs a value")?
                        .parse()
                        .context("--shutdown-seconds is not a number")?
                }
                "--help" | "-h" => {
                    bail!(
                        "cognition_self_learning: --binary <default shenko CLI> \
                         [--repo <source checkout>] [--root <fresh run dir>] \
                         [--authoring-seconds N] [--startup-seconds N] [--shutdown-seconds N]"
                    )
                }
                other_arg => other.push(other_arg.to_owned()),
            }
        }
        let binary = binary.context(
            "the consumer needs --binary pointing at the built default `shenko` CLI",
        )?;
        let repo = match repo {
            Some(repo) => repo,
            None => Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .context("resolve the workspace root as the default --repo")?,
        };
        let root = match root {
            Some(root) => root,
            None => repo
                .join("build/cognition-self-learning")
                .join(format!("run-{}", uuid::Uuid::new_v4())),
        };
        Ok(Self {
            binary,
            repo,
            root,
            authoring: Duration::from_secs(authoring.clamp(60, 7_200)),
            startup: Duration::from_secs(startup.clamp(30, 1_800)),
            shutdown: Duration::from_secs(shutdown.clamp(10, 600)),
            other_args: other,
        })
    }

    /// The real model the run needs. A consumer without a served model cannot
    /// produce a measurement, so it reports the missing prerequisite instead of
    /// inventing one.
    pub fn served_model() -> Option<(String, String)> {
        if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
            return None;
        }
        let url = std::env::var("SHENKO_LOCAL_MODEL_URL").ok()?;
        let model = std::env::var("SHENKO_LOCAL_MODEL").ok()?;
        if url.is_empty() || model.is_empty() {
            return None;
        }
        Some((url, model))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn run_bounded(
    mut command: Command,
    timeout: Duration,
) -> Result<(Option<i32>, String, String, u64, bool)> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command.spawn().context("spawn bounded command")?;
    loop {
        if child.try_wait().context("poll bounded command")?.is_some() {
            break;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("bounded command exceeded {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().context("collect bounded command output")?;
    let elapsed = started.elapsed().as_millis() as u64;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        elapsed,
        output.status.success(),
    ))
}

/// The declared source binding of this run.
///
/// This is what the caller asserted, not a proof of build provenance: the
/// binary's sha256, each bound source file's sha256 (or `absent`), and the
/// repository HEAD. `git` failures are recorded, never replaced by a guess.
pub fn source_binding(options: &Options) -> Value {
    let binary = std::fs::read(&options.binary).ok().map(|bytes| {
        json!({"bytes": bytes.len(), "sha256": sha256_hex(&bytes)})
    });
    let mut sources = serde_json::Map::new();
    for relative in BOUND_SOURCES {
        let path = options.repo.join(relative);
        let entry = match std::fs::read(&path) {
            Ok(bytes) => json!({"bytes": bytes.len(), "sha256": sha256_hex(&bytes)}),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({"absent": true}),
            Err(error) => json!({"unreadable": format!("{error}")}),
        };
        sources.insert((*relative).to_owned(), entry);
    }
    let consumer_sources = ["crates/shenko-exec-host/tests/cognition_self_learning.rs",
        "crates/shenko-exec-host/tests/cognition_self_learning/harness.rs",
        "crates/shenko-exec-host/tests/cognition_self_learning/facts.rs"];
    for relative in consumer_sources {
        let path = options.repo.join(relative);
        let entry = match std::fs::read(&path) {
            Ok(bytes) => json!({"bytes": bytes.len(), "sha256": sha256_hex(&bytes)}),
            Err(_) => json!({"absent": true}),
        };
        sources.insert(relative.to_owned(), entry);
    }
    let mut contract_sources = serde_json::Map::new();
    for relative in CONTRACT_SOURCES {
        let path = options.repo.join(relative);
        let entry = match std::fs::read(&path) {
            Ok(bytes) => json!({"bytes": bytes.len(), "sha256": sha256_hex(&bytes)}),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({"absent": true}),
            Err(error) => json!({"unreadable": format!("{error}")}),
        };
        contract_sources.insert((*relative).to_owned(), entry);
    }
    let head = {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&options.repo)
            .arg("--no-optional-locks")
            .arg("rev-parse")
            .arg("HEAD");
        match run_bounded(command, Duration::from_secs(60)) {
            Ok((code, stdout, stderr, elapsed, success)) => json!({
                "exit_code": code,
                "success": success,
                "seconds": elapsed,
                "stdout": stdout.trim(),
                "stderr": stderr.trim(),
            }),
            Err(error) => json!({"error": format!("{error:#}")}),
        }
    };
    let bound = json!({"sources": &sources, "contract_sources": &contract_sources});
    let sources_digest = blake3_hex(&serde_json::to_vec(&bound).unwrap_or_default());
    json!({
        "binary_path": options.binary.display().to_string(),
        "binary": binary,
        "repo": options.repo.display().to_string(),
        "repo_head": head,
        "sources": sources,
        "contract_sources": contract_sources,
        "blake3_of_sources": sources_digest,
    })
}

/// One CLI start under test.
pub struct Server {
    child: Child,
    pub pid: u32,
    pub port: u16,
    pub phase: String,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

/// What actually ended one CLI start.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StopReport {
    pub phase: String,
    pub pid: u32,
    pub signal: String,
    pub waited_ms: u64,
    pub exit_code: Option<i32>,
    pub exit_success: Option<bool>,
    pub forced: bool,
    pub stdout_path: String,
    pub stderr_path: String,
    pub stderr_tail: String,
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind a loopback port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

impl Server {
    /// Start the real default CLI on one installation.
    pub fn start(options: &Options, data_dir: &Path, phase: &str) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        let port = free_port()?;
        let stdout_path = options.root.join(format!("{phase}.stdout"));
        let stderr_path = options.root.join(format!("{phase}.stderr"));
        let stdout = std::fs::File::create(&stdout_path)?;
        let stderr = std::fs::File::create(&stderr_path)?;
        let child = Command::new(&options.binary)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--log")
            .arg("info")
            .arg("start")
            .arg("--port")
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("start the default CLI {}", options.binary.display()))?;
        let pid = child.id();
        Ok(Self {
            child,
            pid,
            port,
            phase: phase.to_owned(),
            stdout_path,
            stderr_path,
        })
    }

    fn stderr_tail(&self) -> String {
        let bytes = std::fs::read(&self.stderr_path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let start = text.len().saturating_sub(4_096);
        text[start..].to_owned()
    }

    /// Wait until the real UI answers its own health route.
    pub async fn wait_ready(&mut self, timeout: Duration) -> Result<Value> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().context("poll the CLI process")? {
                bail!(
                    "the default CLI exited before it became ready ({status}); stderr tail: {}",
                    self.stderr_tail()
                );
            }
            match get_json(self.port, "/health", Duration::from_secs(5)).await {
                Ok(value) if value.get("status").and_then(Value::as_str) == Some("ok") => {
                    return Ok(json!({
                        "phase": self.phase,
                        "port": self.port,
                        "pid": self.pid,
                        "ready_ms": started.elapsed().as_millis() as u64,
                        "health": value,
                        "startup_seconds_allowed": timeout.as_secs(),
                    }));
                }
                _ => {}
            }
            if started.elapsed() > timeout {
                bail!(
                    "the default CLI did not become ready within {timeout:?}; stderr tail: {}",
                    self.stderr_tail()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Stop it through the real SIGINT shutdown the CLI installs, and report
    /// exactly what ended the process.
    pub async fn stop(&mut self, timeout: Duration) -> Result<StopReport> {
        let mut signal = "sigint".to_owned();
        let mut forced = false;
        let sent = Command::new("kill")
            .arg("-INT")
            .arg(self.pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("send SIGINT to the default CLI")?;
        if !sent.success() {
            signal = format!("sigint_unavailable(exit {})", sent.code().unwrap_or(-1));
        }
        let started = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().context("poll the CLI process")? {
                break Some(status);
            }
            if started.elapsed() > timeout {
                forced = true;
                signal = "sigkill_after_timeout".to_owned();
                let _ = self.child.kill();
                break self.child.wait().ok();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        Ok(StopReport {
            phase: self.phase.clone(),
            pid: self.pid,
            signal,
            waited_ms: started.elapsed().as_millis() as u64,
            exit_code: status.and_then(|status| status.code()),
            exit_success: status.map(|status| status.success()),
            forced,
            stdout_path: self.stdout_path.display().to_string(),
            stderr_path: self.stderr_path.display().to_string(),
            stderr_tail: self.stderr_tail(),
        })
    }
}

/// `GET <path>` on the running CLI, read as JSON.
///
/// The consumer speaks plain HTTP/1.1 over loopback so the harness needs no
/// HTTP client dependency beyond what the workspace already carries.
pub async fn get_json(port: u16, path: &str, timeout: Duration) -> Result<Value> {
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .with_context(|| format!("connect to 127.0.0.1:{port}"))?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\naccept: application/json\r\nconnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        decode_http_response(&raw)
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .context("HTTP request to the default CLI timed out")?
}

fn decode_http_response(raw: &[u8]) -> Result<Value> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("the HTTP response has no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body = raw[split + 4..].to_vec();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    ensure!(
        status_line.contains(" 200"),
        "unexpected HTTP status line: {status_line}"
    );
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
        if name == "content-length" {
            content_length = value.parse::<usize>().ok();
        }
    }
    if let Some(length) = content_length {
        body.truncate(length);
    } else if chunked {
        body = decode_chunked(&body)?;
    }
    serde_json::from_slice(&body).context("decode the CLI JSON body")
}

fn decode_chunked(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    loop {
        let Some(line_end) = bytes[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
        else {
            bail!("truncated chunked body");
        };
        let header = String::from_utf8_lossy(&bytes[cursor..cursor + line_end]).to_string();
        let size_text = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("chunk size '{size_text}' is not hexadecimal"))?;
        cursor += line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        ensure!(cursor + size <= bytes.len(), "chunked chunk exceeds the body");
        out.extend_from_slice(&bytes[cursor..cursor + size]);
        cursor += size + 2;
    }
}

/// Submit one user message through the product's own web-input socket.
///
/// The receipt is the product's answer: `stage` says whether the input was
/// durably admitted. A rejected or unavailable stage is returned as it came
/// back, never rewritten into success.
pub async fn submit(
    port: u16,
    subject_id: &str,
    content: &str,
    timeout: Duration,
) -> Result<Value> {
    let submission_id = uuid::Uuid::new_v4().to_string();
    let message_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({
        "schema_version": 2,
        "type": "submit",
        "submission_id": submission_id,
        "message_id": message_id,
        "subject_id": subject_id,
        "mode": "append",
        "content": content,
        "attachment_refs": [],
        "rich_content_ref": Value::Null,
        "observed_step_id": Value::Null,
        "observed_request_id": Value::Null,
    });
    let exchange = async {
        let (mut socket, response) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
                .await
                .context("open the default CLI websocket")?;
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                payload.to_string().into(),
            ))
            .await
            .context("send the user submission")?;
        loop {
            let Some(message) = socket.next().await else {
                bail!("the websocket closed before the input receipt arrived");
            };
            let message = message.context("read the websocket")?;
            let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if value.get("kind").and_then(Value::as_str) == Some("receipt")
                && value.get("submission_id").and_then(Value::as_str)
                    == Some(submission_id.as_str())
            {
                let _ = socket.close(None).await;
                return Ok(json!({
                    "submission_id": submission_id,
                    "message_id": message_id,
                    "receipt": value,
                    "handshake_status": response.status().as_u16(),
                }));
            }
        }
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .context("the websocket input receipt did not arrive in time")?
}
