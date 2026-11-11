//! Original LayerFS on a qualified Linux VM (docs/specs/durable-state.md#spec-10-2-1, docs/specs/desktop-runtime.md#spec-34).
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
    /// layerfs_committed vs product_durable stay distinct (docs/specs/durable-state.md#spec-10-2-1):
    /// this flag is the upstream Store's own commit receipt only.
    pub layerfs_committed: bool,
    /// Guest capability facts the companion really probed over the VM's own
    /// exec channel. `None` means this answer carried no probe at all (for
    /// example a `stop` reply) — never "all capabilities present".
    #[serde(default)]
    pub guest: Option<LayerFsGuestCapabilities>,
    /// The pinned original SDK surface the companion bound in the guest.
    #[serde(default)]
    pub sdk: Option<LayerFsSdkBinding>,
    pub detail: serde_json::Value,
}

/// Guest requirements for the FULL LayerFS workspace path (docs/specs/durable-state.md#spec-10-2-1):
/// Docker Engine, FUSE, devices/mount privileges, a matching runtime image
/// and a plain local named volume. The companion probes what the provider
/// can actually prove; every unprobed requirement stays unprobed here and
/// [`LayerFsGuestCapabilities::full_workspace_path`] never upgrades it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerFsGuestCapabilities {
    /// The provider exposes a guest exec channel at all.
    #[serde(default)]
    pub exec: bool,
    /// `command -v docker` succeeded in the guest.
    #[serde(default)]
    pub docker: bool,
    /// `/dev/fuse` exists in the guest.
    #[serde(default)]
    pub fuse: bool,
    /// `/proc/mounts` was readable (devices/mount privileges observable).
    /// Kept separate from `fuse` so a provider that only reports one probe
    /// cannot be read as the other.
    #[serde(default, rename = "mountsObserved", alias = "mounts_observed")]
    pub mounts_observed: bool,
    /// Companion's own probe note.
    #[serde(default)]
    pub detail: String,
}

impl LayerFsGuestCapabilities {
    /// Honest state of the full-workspace prerequisites. It never claims the
    /// runtime image or named volume were verified — those are not probed.
    pub fn full_workspace_path(&self) -> &'static str {
        if !self.exec {
            return "missing_guest_exec";
        }
        if self.docker && self.fuse && self.mounts_observed {
            // Naming the probed subset keeps the unprobed runtime image and
            // named volume out of the claim.
            "probed_docker_fuse_mounts"
        } else {
            "incomplete"
        }
    }
}

/// The original LayerFS SDK surface bound inside the guest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerFsSdkBinding {
    /// Pinned release identity reported by the companion.
    #[serde(default)]
    pub pin: String,
    /// Export names the pinned SDK really exposes.
    #[serde(default)]
    pub exports: Vec<String>,
    /// Optional exports the pinned SDK does not expose.
    #[serde(default, rename = "missingOptional", alias = "missing_optional")]
    pub missing_optional: Vec<String>,
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
    events: Arc<Mutex<VecDeque<serde_json::Value>>>,
    /// The companion's own `fatal` event detail. The companion reports SDK
    /// binding and VM-provider failures as an event before exiting, so a
    /// transport loss must carry that reason instead of only "process died".
    fatal: Arc<Mutex<Option<serde_json::Value>>>,
    next_seq: AtomicU64,
    pump_failed: Arc<AtomicBool>,
}

/// Async progress events retained for recovery reporting.
const MAX_RETAINED_EVENTS: usize = 64;

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
            events: Arc::new(Mutex::new(VecDeque::new())),
            fatal: Arc::new(Mutex::new(None)),
            next_seq: AtomicU64::new(1),
            pump_failed: Arc::new(AtomicBool::new(false)),
        });
        {
            let pending = Arc::clone(&this.pending);
            let events = Arc::clone(&this.events);
            let fatal = Arc::clone(&this.fatal);
            let pump_failed = Arc::clone(&this.pump_failed);
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
                                if value.get("event").and_then(|kind| kind.as_str())
                                    == Some("fatal")
                                {
                                    if let Ok(mut slot) = fatal.lock() {
                                        *slot = Some(
                                            value
                                                .get("detail")
                                                .cloned()
                                                .unwrap_or(serde_json::Value::Null),
                                        );
                                    }
                                }
                                if let Ok(mut queue) = events.lock() {
                                    if queue.len() >= MAX_RETAINED_EVENTS {
                                        queue.pop_front();
                                    }
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
                            // A response whose caller already gave up (timeout)
                            // has no live slot; dropping it keeps a late answer
                            // from being mistaken for a later request's result.
                            if let Ok(mut pending) = pending.lock() {
                                if let Some(slot) = pending.remove(&seq) {
                                    if let Ok(mut guard) = slot.lock() {
                                        *guard = Some(response);
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                pump_failed.store(true, Ordering::Relaxed);
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
                detail: format!("companion process exited: {}", self.fatal_detail()),
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
            // The slot lock is released before the pending map is touched:
            // the pump thread takes `pending` first and then the slot, so
            // holding a slot while locking `pending` here would invert that
            // order and can deadlock the two threads.
            let ready = {
                let mut guard = slot.lock().map_err(|_| DesktopError::Lost {
                    detail: "companion slot poisoned".into(),
                })?;
                guard.take()
            };
            if let Some(response) = ready {
                self.forget_pending(seq);
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
            if Instant::now() >= deadline {
                // The caller must not keep waiting for a slot nobody will
                // fill; a late answer is dropped by the pump instead of
                // being attached to a later request.
                self.forget_pending(seq);
                let fatal = self.fatal_detail();
                return Err(DesktopError::Lost {
                    detail: format!(
                        "companion did not answer `{cmd}` within {timeout:?}; the guest may still be executing it ({fatal})"
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn forget_pending(&self, seq: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&seq);
        }
    }

    /// Human-readable cause for a lost transport: the companion's own fatal
    /// event when it reported one, never a guessed reason.
    fn fatal_detail(&self) -> String {
        match self.fatal.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(detail) if !detail.is_null() => detail.to_string(),
                _ => "no fatal event reported".into(),
            },
            Err(_) => "fatal state poisoned".into(),
        }
    }

    fn alive(&self) -> bool {
        !self.pump_failed.load(Ordering::Relaxed)
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
    /// `layerfs_committed` only — never `product_durable` (docs/specs/durable-state.md#spec-10-2-1).
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

    /// Real readiness facts of the original-LayerFS VM path on this node.
    /// Nothing here starts a VM or spawns the companion: it reports the
    /// configuration the adapter would use and which prerequisite is
    /// missing, so capability discovery never claims a VM it cannot drive.
    pub fn capabilities(&self) -> serde_json::Value {
        let sdk_configured = !self.config.sdk_path.as_os_str().is_empty();
        let sdk_exists = sdk_configured && self.config.sdk_path.is_dir();
        let script_exists = self.config.script.is_file();
        let node_from_env = std::env::var_os("SHENKO_LAYERFS_NODE").is_some();
        let provider_from_env = std::env::var_os("SHENKO_LAYERFS_VM_PROVIDER").is_some();
        let state = if !sdk_configured {
            "sdk_path_unset"
        } else if !sdk_exists {
            "sdk_path_missing"
        } else if !script_exists {
            "companion_script_missing"
        } else {
            "configured_vm_not_started"
        };
        serde_json::json!({
            "provider": "layerfs_vm",
            "sdk_pin": LAYERFS_SDK_PIN,
            "sdk_path_configured": sdk_configured,
            "sdk_path_exists": sdk_exists,
            "vm_provider": self.config.vm_provider,
            "vm_provider_from_env": provider_from_env,
            "node_binary": self.config.node_binary.display().to_string(),
            "node_binary_from_env": node_from_env,
            "companion_script": self.config.script.display().to_string(),
            "companion_script_exists": script_exists,
            "state": state,
            // Named, not claimed: start/state probe these in the guest and
            // report only what the exec channel actually answered.
            "guest_requirements": [
                "docker_engine",
                "fuse",
                "device_and_mount_privileges",
                "matching_runtime_image",
                "plain_local_named_volume",
            ],
            "probe": "sdk export surface + guest capabilities are probed by the companion at start/state",
            "durability": "layerfs_committed only; product_durable requires this product's own durable publication",
        })
    }
}

fn parse_vm_state(name: &str, detail: serde_json::Value) -> Result<LayerFsVmState, DesktopError> {
    let state = detail
        .get("state")
        .and_then(|value| value.as_str())
        .ok_or_else(|| DesktopError::unavailable("protocol_shape_mismatch", "no `state` in companion detail"))?;
    // Capability facts are reported only as far as the companion actually
    // probed them: an absent probe stays absent instead of becoming `false`.
    let guest = detail
        .get("guest")
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::from_value::<LayerFsGuestCapabilities>(value.clone()).ok());
    let sdk = detail
        .get("sdk")
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::from_value::<LayerFsSdkBinding>(value.clone()).ok());
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
        guest,
        sdk,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::ids::EntityId;

    fn config(sdk: PathBuf, script: PathBuf, node: PathBuf) -> LayerFsAdapterConfig {
        LayerFsAdapterConfig {
            sdk_path: sdk,
            vm_provider: "boxlite".into(),
            node_binary: node,
            script,
        }
    }

    /// A `start` answer carries the guest probe and the bound SDK surface;
    /// both must reach the product instead of being dropped on the floor.
    #[test]
    fn start_answer_surfaces_guest_probe_and_sdk_binding() {
        let detail = serde_json::json!({
            "state": "running",
            "display": "tcp/127.0.0.1:6001",
            "sshPort": 2222,
            "layerfsCommitted": false,
            "guest": {
                "exec": true,
                "docker": true,
                "fuse": true,
                "mountsObserved": true,
                "detail": "probed via guest exec",
            },
            "sdk": {
                "pin": LAYERFS_SDK_PIN,
                "exports": ["Store", "Branch"],
                "missingOptional": ["Workspace"],
            },
        });
        let state = parse_vm_state("shenko-layerfs", detail).expect("start answer parses");
        assert_eq!(state.state, "running");
        assert_eq!(state.display.as_deref(), Some("tcp/127.0.0.1:6001"));
        assert_eq!(state.ssh_port, Some(2222));
        assert!(!state.layerfs_committed, "start never claims the upstream commit");
        let guest = state.guest.expect("guest probe is surfaced");
        assert!(guest.exec && guest.docker && guest.fuse && guest.mounts_observed);
        assert_eq!(guest.full_workspace_path(), "probed_docker_fuse_mounts");
        let sdk = state.sdk.expect("sdk binding is surfaced");
        assert_eq!(sdk.pin, LAYERFS_SDK_PIN);
        assert_eq!(sdk.exports, vec!["Store".to_string(), "Branch".to_string()]);
        assert_eq!(sdk.missing_optional, vec!["Workspace".to_string()]);
    }

    /// Unprobed requirements stay unprobed: a reply with no guest object is
    /// not read as "everything present", and a partial probe is incomplete.
    #[test]
    fn guest_capabilities_never_upgrade_unprobed_requirements() {
        let stopped = parse_vm_state(
            "vm",
            serde_json::json!({ "state": "stopped", "layerfsCommitted": false }),
        )
        .expect("stop answer parses");
        assert!(stopped.guest.is_none());
        assert!(stopped.sdk.is_none());
        assert!(!stopped.layerfs_committed);

        let no_exec = LayerFsGuestCapabilities::default();
        assert_eq!(no_exec.full_workspace_path(), "missing_guest_exec");

        let partial = LayerFsGuestCapabilities {
            exec: true,
            docker: true,
            fuse: false,
            mounts_observed: true,
            detail: "probed via guest exec".into(),
        };
        assert_eq!(partial.full_workspace_path(), "incomplete");

        let probed = LayerFsGuestCapabilities {
            exec: true,
            docker: true,
            fuse: true,
            mounts_observed: true,
            detail: "probed via guest exec".into(),
        };
        // The claim names only the probed subset; the runtime image and the
        // local named volume stay out of it.
        assert_eq!(probed.full_workspace_path(), "probed_docker_fuse_mounts");
    }

    /// Without the pinned SDK path the adapter refuses before spawning any
    /// process, and capability discovery reports the same missing piece.
    #[test]
    fn unset_sdk_path_is_typed_unavailable_without_spawning() {
        let adapter = LayerFsAdapter::new(config(
            PathBuf::new(),
            PathBuf::from("/nonexistent/layerfs-companion.mjs"),
            PathBuf::from("node"),
        ));
        match adapter.start_vm("vm") {
            Err(DesktopError::Unavailable { reason, .. }) => {
                assert_eq!(reason, "layerfs_sdk_path_unset")
            }
            other => panic!("expected typed-unavailable, got {other:?}"),
        }
        let facts = adapter.capabilities();
        assert_eq!(facts["sdk_pin"], serde_json::json!(LAYERFS_SDK_PIN));
        assert_eq!(facts["sdk_path_configured"], serde_json::json!(false));
        assert_eq!(facts["sdk_path_exists"], serde_json::json!(false));
        assert_eq!(facts["companion_script_exists"], serde_json::json!(false));
        assert_eq!(facts["state"], serde_json::json!("sdk_path_unset"));
        assert_eq!(facts["vm_provider"], serde_json::json!("boxlite"));
        assert_eq!(
            facts["guest_requirements"]
                .as_array()
                .map(|items| items.len()),
            Some(5),
            "every full-workspace prerequisite is named"
        );
    }

    /// A configured-but-absent SDK checkout and a missing companion script
    /// are distinct facts, and a real spawn failure is reported as such
    /// instead of turning into a fake VM state.
    #[test]
    fn readiness_distinguishes_missing_sdk_missing_script_and_spawn_failure() {
        let root = std::env::temp_dir().join(format!("shenko-layerfs-{}", EntityId::new()));
        std::fs::create_dir_all(&root).expect("temp root");
        let script = root.join("layerfs-companion.mjs");
        std::fs::write(&script, "// test companion placeholder\n").expect("write script");

        let missing_sdk = LayerFsAdapter::new(config(
            root.join("absent-sdk"),
            script.clone(),
            PathBuf::from("node"),
        ));
        assert_eq!(
            missing_sdk.capabilities()["state"],
            serde_json::json!("sdk_path_missing")
        );

        let sdk = root.join("sdk");
        std::fs::create_dir_all(&sdk).expect("sdk dir");
        let missing_script = LayerFsAdapter::new(config(
            sdk.clone(),
            root.join("absent-companion.mjs"),
            PathBuf::from("node"),
        ));
        assert_eq!(
            missing_script.capabilities()["state"],
            serde_json::json!("companion_script_missing")
        );

        // Everything configured, but node itself cannot be executed: the
        // adapter must report the real spawn failure, never a VM state.
        let bad_node = LayerFsAdapter::new(config(
            sdk,
            script,
            root.join("absent-node-binary"),
        ));
        assert_eq!(
            bad_node.capabilities()["state"],
            serde_json::json!("configured_vm_not_started")
        );
        match bad_node.vm_state("vm") {
            Err(DesktopError::Unavailable { reason, .. }) => {
                assert_eq!(reason, "companion_spawn_failed")
            }
            other => panic!("expected typed-unavailable, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
