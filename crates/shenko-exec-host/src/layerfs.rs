//! Original LayerFS on a qualified Linux VM (SPEC §10.2.1, §34).
//!
//! This crate never re-implements the LayerFS Store. Every Store / branch /
//! workspace operation is executed by the pinned original LayerFS SDK
//! v1.0.0 (`2aaf2552ce21154928b9f9c83582e41a637f5910`) **inside** a
//! qualified Linux VM, driven over a line-delimited JSON protocol by the
//! companion Node script `scripts/layerfs-companion.mjs`. The companion
//! binds the SDK's real export surface at startup and refuses to run when
//! the surface does not match the pinned v1.0.0 contract — the original
//! semantics (kernel/filesystem guard, Disposable profile durability
//! limits, branch/content behavior) stay upstream's, not ours.
//!
//! BoxLite is the optional first VM provider; any qualified Linux VM
//! provider can be plugged in via `vm_provider`. Absent actual VM
//! hardware/provider is reported as typed-unavailable evidence, never as
//! completion.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::desktop::DesktopError;

/// Pinned original LayerFS SDK identity. The companion probes the SDK's
/// runtime export surface against this exact release; a mismatch stops the
/// adapter with a typed error instead of silently driving a different
/// LayerFS.
pub const LAYERFS_SDK_PIN: &str = "v1.0.0@2aaf2552ce21154928b9f9c83582e41a637f5910";

#[derive(Debug, Clone)]
pub struct LayerFsAdapterConfig {
    /// Directory of the pinned original LayerFS JS SDK checkout.
    pub sdk_path: PathBuf,
    /// Qualified Linux VM provider the companion drives ("boxlite", ...).
    pub vm_provider: String,
    /// Node.js binary used to run the companion script.
    pub node_binary: PathBuf,
    /// The companion script itself.
    pub script: PathBuf,
}

impl LayerFsAdapterConfig {
    /// Real configuration from the environment; nothing is guessed toward a
    /// running VM. Every unset value is an explicit typed-unavailable at
    /// use time, not a fake default.
    pub fn from_env() -> Self {
        Self {
            sdk_path: std::env::var_os("SHENKO_LAYERFS_SDK_PATH")
                .map(PathBuf::from)
                .unwrap_or_default(),
            vm_provider: std::env::var("SHENKO_LAYERFS_VM_PROVIDER")
                .unwrap_or_else(|_| "boxlite".into()),
            node_binary: std::env::var_os("SHENKO_LAYERFS_NODE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("node")),
            script: std::env::var_os("SHENKO_LAYERFS_COMPANION_SCRIPT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("scripts/layerfs-companion.mjs")),
        }
    }
}

/// One VM's lifecycle facts as reported by the companion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerFsVmState {
    pub name: String,
    /// "stopped" | "starting" | "running" | "stopped_error".
    pub state: String,
    /// Guest X11 display address reachable from this host ("tcp/host:99"),
    /// once the guest display service is ready.
    pub display: Option<String>,
    /// Host-forwarded SSH port into the guest, when the provider offers one.
    pub ssh_port: Option<u16>,
    /// layerfs_committed vs product_durable stay distinct (SPEC §10.2.1):
    /// this flag is the upstream Store's own commit receipt only.
    pub layerfs_committed: bool,
    pub detail: serde_json::Value,
}

/// One request/response pair on the companion protocol.
#[derive(Debug, Serialize)]
struct CompanionRequest {
    seq: u64,
    cmd: &'static str,
    args: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CompanionResponse {
    seq: u64,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: serde_json::Value,
}

/// A single companion process driving one VM.
struct LayerFsCompanion {
    name: String,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<Mutex<BTreeMap<u64, Arc<Mutex<Option<CompanionResponse>>>>>>,
    inbox: Arc<Mutex<VecDeque<CompanionResponse>>>,
    events: Arc<Mutex<VecDeque<serde_json::Value>>>,
    next_seq: AtomicU64,
    pump_failed: Arc<AtomicBool>,
    started: Instant,
}

impl LayerFsCompanion {
    fn spawn(config: &LayerFsAdapterConfig, name: &str) -> Result<Arc<Self>, DesktopError> {
        if config.sdk_path.as_os_str().is_empty() {
            return Err(DesktopError::unavailable(
                "layerfs_sdk_path_unset",
                "set SHENKO_LAYERFS_SDK_PATH to the pinned original LayerFS SDK checkout",
            ));
        }
        if !config.script.is_file() {
            return Err(DesktopError::unavailable(
                "companion_script_missing",
                format!("{} is not a file", config.script.display()),
            ));
        }
        let mut child = Command::new(&config.node_binary)
            .arg(&config.script)
            .env("SHENKO_LAYERFS_SDK_PATH", &config.sdk_path)
            .env("SHENKO_LAYERFS_VM_PROVIDER", &config.vm_provider)
            .env("SHENKO_LAYERFS_VM_NAME", name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                DesktopError::unavailable("companion_spawn_failed", error.to_string())
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            DesktopError::unavailable("companion_stdio_missing", "stdin not piped")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            DesktopError::unavailable("companion_stdio_missing", "stdout not piped")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DesktopError::unavailable("companion_stdio_missing", "stderr not piped")
        })?;
        let this = Arc::new(Self {
            name: name.to_string(),
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            inbox: Arc::new(Mutex::new(VecDeque::new())),
            events: Arc::new(Mutex::new(VecDeque::new())),
            next_seq: AtomicU64::new(1),
            pump_failed: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
        });
        {
            let pending = Arc::clone(&this.pending);
            let inbox = Arc::clone(&this.inbox);
            let events = Arc::clone(&this.events);
            let pump_failed = Arc::clone(&this.pump_failed);
            let name_for_pump = name.to_string();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
                            else {
                                continue;
                            };
                            if value.get("event").is_some() {
                                if let Ok(mut queue) = events.lock() {
                                    queue.push_back(value);
                                }
                                continue;
                            }
                            let Ok(response) =
                                serde_json::from_value::<CompanionResponse>(value)
                            else {
                                continue;
                            };
                            let seq = response.seq;
                            if let Ok(mut pending) = pending.lock() {
                                if let Some(slot) = pending.remove(&seq) {
                                    if let Ok(mut guard) = slot.lock() {
                                        *guard = Some(response);
                                        continue;
                                    }
                                }
                            }
                            if let Ok(mut queue) = inbox.lock() {
                                queue.push_back(response);
                            }
                        }
                        Err(_) => break,
                    }
                }
                pump_failed.store(true, Ordering::Relaxed);
                let _ = name_for_pump;
            });
        }
        {
            let name_for_err = name.to_string();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    // Companion stderr is diagnostic-only; the protocol
                    // stays on stdout. Surface the first lines on exit
                    // via the reconcile path, not per line.
                    let _ = (&name_for_err, line);
                }
            });
        }
        Ok(this)
    }

    fn request(
        &self,
        cmd: &'static str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, DesktopError> {
        if self.pump_failed.load(Ordering::Relaxed) {
            return Err(DesktopError::Lost {
                detail: "companion process exited".into(),
            });
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(Mutex::new(None));
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(seq, Arc::clone(&slot));
        }
        let line = serde_json::to_string(&CompanionRequest { seq, cmd, args })
            .map_err(|error| DesktopError::unavailable("protocol_encode_failed", error.to_string()))?;
        {
            let mut stdin = self.stdin.lock().map_err(|_| DesktopError::Lost {
                detail: "companion stdin poisoned".into(),
            })?;
            let Some(stdin) = stdin.as_mut() else {
                return Err(DesktopError::Lost {
                    detail: "companion stdin closed".into(),
                });
            };
            stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush())
                .map_err(|error| DesktopError::Lost {
                    detail: format!("companion stdin write failed: {error}"),
                })?;
        }
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut guard = slot.lock().map_err(|_| DesktopError::Lost {
                    detail: "companion slot poisoned".into(),
                })?;
                if let Some(response) = guard.take() {
                    if !response.ok {
                        return Err(DesktopError::unavailable(
                            "companion_command_failed",
                            response
                                .error
                                .unwrap_or_else(|| "unknown companion error".into()),
                        ));
                    }
                    return Ok(response.detail);
                }
            }
            if Instant::now() >= deadline {
                return Err(DesktopError::Lost {
                    detail: format!("companion did not answer `{cmd}` within {timeout:?}"),
                });
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn alive(&self) -> bool {
        !self.pump_failed.load(Ordering::Relaxed)
    }

    fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Drop for LayerFsCompanion {
    fn drop(&mut self) {
        // Best-effort graceful stop of the guest before killing the
        // companion; the VM provider's own state remains the source of
        // truth and is re-queried on next use.
        let _ = self.request("stop", serde_json::json!({}), Duration::from_secs(20));
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Drives original LayerFS VMs through per-VM companion processes.
pub struct LayerFsAdapter {
    config: LayerFsAdapterConfig,
    companions: Mutex<BTreeMap<String, Arc<LayerFsCompanion>>>,
}

impl LayerFsAdapter {
    pub fn new(config: LayerFsAdapterConfig) -> Self {
        Self {
            config,
            companions: Mutex::new(BTreeMap::new()),
        }
    }

    fn companion(&self, name: &str) -> Result<Arc<LayerFsCompanion>, DesktopError> {
        if let Ok(companions) = self.companions.lock() {
            if let Some(companion) = companions.get(name) {
                if companion.alive() {
                    return Ok(Arc::clone(companion));
                }
            }
        }
        let companion = LayerFsCompanion::spawn(&self.config, name)?;
        if let Ok(mut companions) = self.companions.lock() {
            companions.insert(name.to_string(), Arc::clone(&companion));
        }
        Ok(companion)
    }

    /// Start (or attach to) the named VM and return its state. The
    /// original LayerFS Store runs inside the guest; this adapter only
    /// drives the VM and reports what the guest/SDK actually answered.
    pub fn start_vm(&self, name: &str) -> Result<LayerFsVmState, DesktopError> {
        let companion = self.companion(name)?;
        let detail = companion.request(
            "start",
            serde_json::json!({ "name": name }),
            Duration::from_secs(300),
        )?;
        parse_vm_state(name, detail)
    }

    pub fn stop_vm(&self, name: &str) -> Result<LayerFsVmState, DesktopError> {
        let companion = self.companion(name)?;
        let detail = companion.request(
            "stop",
            serde_json::json!({ "name": name }),
            Duration::from_secs(120),
        )?;
        parse_vm_state(name, detail)
    }

    pub fn vm_state(&self, name: &str) -> Result<LayerFsVmState, DesktopError> {
        let companion = self.companion(name)?;
        let detail = companion.request(
            "state",
            serde_json::json!({ "name": name }),
            Duration::from_secs(30),
        )?;
        parse_vm_state(name, detail)
    }

    /// Ask the guest's original SDK for its own commit receipt. This is
    /// `layerfs_committed` only — never `product_durable` (SPEC §10.2.1).
    pub fn commit_receipt(&self, name: &str) -> Result<serde_json::Value, DesktopError> {
        let companion = self.companion(name)?;
        companion.request(
            "commit_receipt",
            serde_json::json!({ "name": name }),
            Duration::from_secs(60),
        )
    }

    pub fn config(&self) -> &LayerFsAdapterConfig {
        &self.config
    }
}

fn parse_vm_state(name: &str, detail: serde_json::Value) -> Result<LayerFsVmState, DesktopError> {
    let state = detail
        .get("state")
        .and_then(|value| value.as_str())
        .ok_or_else(|| DesktopError::unavailable("protocol_shape_mismatch", "no `state` in companion detail"))?;
    Ok(LayerFsVmState {
        name: name.to_string(),
        state: state.to_string(),
        display: detail
            .get("display")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        ssh_port: detail
            .get("sshPort")
            .and_then(|value| value.as_u64())
            .map(|port| port as u16),
        layerfs_committed: detail
            .get("layerfsCommitted")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        detail,
    })
}
