//! On-device bridge client — the only seam between the node runtime and the
//! Android mechanisms, which live in the APK's app process (Shizuku binders,
//! Dhizuku grant, AccessibilityService, `su` argv exec).
//!
//! The bridge is a same-uid, app-owned UNIX-socket JSON-lines service:
//! - Authentication: the bridge service writes a random per-incarnation
//!   token to `<data_dir>/device_bridge.token` (0600, app-private) at start;
//!   the node presents it as the first frame. The token binds the *bridge
//!   service incarnation* (app process + service lifecycle); it grants no OS
//!   authority and never elevates any operation. Every op still flows through
//!   the real OS mechanism with its own live grant check.
//! - Transport: one handshake frame + one request line + one response line
//!   per connection; bounds: request ≤ 64 KiB, response ≤ 16 MiB (screen PNG),
//!   handshake ≤ 4 KiB, enforced both sides *while reading*, so a peer that
//!   never sends LF cannot make the node buffer past the bound. The node
//!   connects per round-trip; no long-lived socket, so app-process death
//!   surfaces as an immediate connect failure, and a wedged app process
//!   surfaces as a bounded connect timeout instead of a pinned worker.
//! - Replaceability: `DeviceBridge` is the seam; `SocketBridge` is the real
//!   on-device client, `UnavailableBridge` the honest off-device answer. A
//!   future in-process bridge (single-process hosting) implements the same
//!   trait without touching handlers.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bridge wire version; both sides refuse mismatched versions.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// The handshake reply is a fixed small envelope; the device side bounds it
/// at the same 4 KiB, so a larger reply is not a handshake.
const MAX_HANDSHAKE_BYTES: usize = 4 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Which real mechanism a bridge operation asks for.
///
/// Auto lets the device side pick the best *actually granted* mechanism and
/// report which one it used; an explicit variant requires exactly that
/// mechanism and fails typed-unavailable if it is not granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeAuthority {
    Auto,
    RootSu,
    Shizuku,
    Dhizuku,
    Accessibility,
}

impl BridgeAuthority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::RootSu => "root_su",
            Self::Shizuku => "shizuku",
            Self::Dhizuku => "dhizuku",
            Self::Accessibility => "accessibility",
        }
    }
}

/// One device-side input action (mirrors the handler action vocabulary).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeInputAction {
    Tap { x: f64, y: f64 },
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        duration_ms: u64,
    },
    Key { keycode: i32, down: bool },
    Text { text: String },
    Wait { ms: u64 },
}

/// All device operations the bridge service implements.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BridgeOp {
    /// Device + authority capability facts, OS/app versions, binding digest.
    Probe,
    /// Cached adapter states from the device side.
    CapabilityList,
    /// Capture the display into a PNG through a granted mechanism.
    ScreenCapture {
        display_id: i32,
        region: Option<[i32; 4]>,
    },
    /// Fingerprint the semantic node hierarchy the enabled accessibility
    /// service can actually see on this display, inside `region`.
    ///
    /// The device traverses the real nodes, hashes a canonical projection of
    /// them (no node text ever comes back) and answers with the digest, the
    /// bounds it applied, the windows/packages it covered and whether the
    /// traversal stopped at one of those bounds. Absence is an error frame:
    /// there is no digest to compare when the hierarchy is unreachable.
    NodeTreeFingerprint {
        display_id: i32,
        /// Display-absolute rectangle; the node set is filtered by it.
        region: [i32; 4],
        /// Traversal bounds the device must apply and echo back. A device that
        /// answers different bounds has not described the same scope.
        max_nodes: u32,
        max_depth: u32,
        max_windows: u32,
    },
    /// Apply one input batch. Lease/epoch checks happen node-side before the
    /// op is ever sent; the device side replays its own grant checks.
    InputBatch {
        actions: Vec<BridgeInputAction>,
    },
    /// Run frozen argv through the requested mechanism. No shell string is
    /// ever constructed; argv is passed through verbatim.
    ShellExec {
        authority: BridgeAuthority,
        argv: Vec<String>,
        env: Vec<String>,
        timeout_secs: u64,
    },
    /// Produce a local byte-level copy of an app database file set into
    /// app-private storage and report what was actually copied.
    DbSnapshot {
        authority: BridgeAuthority,
        package: String,
        db_name: String,
        /// Prefer a specific consistency method; Auto picks the strongest
        /// available and reports the truth.
        method: BridgeSnapshotMethod,
    },
    /// Accessibility human-activity feed: events observed after a timestamp.
    HumanActivityAfter { after_ms: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeSnapshotMethod {
    Auto,
    /// Copy the complete file set (main + WAL/SHM/journal companions). Not a
    /// consistent snapshot; the node records it only as `snapshot_recovered_copy`.
    RawFileSetCopy,
    /// Only valid when the source is directly readable by the node uid; the
    /// node then runs the SQLite backup API itself. The device side returns
    /// `direct_readable_path` instead of copying.
    DirectReadable,
    /// App-supported export / content-provider surface.
    AppExport,
}

/// Structured bridge failure. `kind` is stable vocabulary the handlers map
/// to outcomes/effects; `message` is diagnostic text, never authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeError {
    pub kind: BridgeErrorKind,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeErrorKind {
    /// The mechanism is absent, not granted, or this op is outside what the
    /// mechanism can honestly do. Never retried blindly.
    TypedUnavailable,
    /// The OS or service denied the operation.
    Denied,
    /// Local I/O (socket, file, process spawn).
    Io,
    /// Op vocabulary/parameter the device side does not implement.
    Unsupported,
    /// The operation ran past its deadline.
    Timeout,
    /// Anything else, with `retryable` judgment.
    Internal,
}

impl BridgeError {
    pub fn typed_unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::TypedUnavailable,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self {
            kind: BridgeErrorKind::Io,
            message: message.into(),
            retryable: true,
        }
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for BridgeError {}

/// The device-bridge seam. Implementations: `SocketBridge` (real, on-device),
/// `UnavailableBridge` (honest off-device), future in-process variants.
pub trait DeviceBridge: Send + Sync {
    fn roundtrip(&self, op: BridgeOp) -> Result<serde_json::Value, BridgeError>;
}

/// Real on-device client: UNIX socket at `<data_dir>/device_bridge.sock`
/// with the token from `<data_dir>/device_bridge.token`.
pub struct SocketBridge {
    socket_path: PathBuf,
    token_path: PathBuf,
}

#[derive(Serialize)]
struct BridgeRequest<'a> {
    version: u32,
    request_id: &'a str,
    // BridgeOps dispatches the internally tagged operation at the root.
    #[serde(flatten)]
    operation: &'a BridgeOp,
}

impl SocketBridge {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            socket_path: data_dir.join("device_bridge.sock"),
            token_path: data_dir.join("device_bridge.token"),
        }
    }

    fn op_timeout(op: &BridgeOp) -> Duration {
        match op {
            BridgeOp::ShellExec { timeout_secs, .. } => {
                Duration::from_secs(u64::from(*timeout_secs).saturating_add(15).min(90))
            }
            // The device aborts its own traversal with a typed timeout error
            // before this deadline; the deadline itself is the bound for a
            // wedged accessibility IPC, not for the traversal.
            BridgeOp::NodeTreeFingerprint { .. } => Duration::from_secs(20),
            BridgeOp::InputBatch { actions } => {
                let waits: u64 = actions
                    .iter()
                    .map(|a| match a {
                        BridgeInputAction::Wait { ms } => *ms,
                        BridgeInputAction::Swipe { duration_ms, .. } => *duration_ms,
                        _ => 0,
                    })
                    .sum();
                Duration::from_millis(waits.saturating_add(10_000))
            }
            _ => Duration::from_secs(30),
        }
    }

    fn io_unavailable(message: String) -> BridgeError {
        BridgeError {
            kind: BridgeErrorKind::TypedUnavailable,
            message,
            retryable: true,
        }
    }
}

/// One bounded wire frame read from the device side.
///
/// The bound counts UTF-8 bytes and is enforced *while* reading, so a peer
/// that never sends LF, or sends an oversized frame, cannot make the node
/// buffer without limit. A frame is decoded only once complete, so a
/// multibyte code point split across reads cannot alter text or argv.
#[derive(Debug)]
enum FrameError {
    /// The peer closed the connection before the frame's LF.
    Closed,
    /// More than `cap` bytes arrived before LF.
    Oversized { cap: usize },
    /// The complete frame is not valid UTF-8.
    Malformed,
    Io(std::io::Error),
}

fn read_frame<R: BufRead>(reader: &mut R, cap: usize) -> Result<String, FrameError> {
    let mut frame: Vec<u8> = Vec::with_capacity(cap.min(1024));
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(FrameError::Io(error)),
        };
        if available.is_empty() {
            return Err(FrameError::Closed);
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                if frame.len() + index > cap {
                    return Err(FrameError::Oversized { cap });
                }
                frame.extend_from_slice(&available[..index]);
                reader.consume(index + 1);
                return std::str::from_utf8(&frame)
                    .map(str::to_string)
                    .map_err(|_| FrameError::Malformed);
            }
            None => {
                let length = available.len();
                // Even if the next byte were LF, this frame would already be
                // longer than the bound.
                if frame.len() + length > cap {
                    return Err(FrameError::Oversized { cap });
                }
                frame.extend_from_slice(available);
                reader.consume(length);
            }
        }
    }
}

/// `UnixStream::connect` blocks when the listener's backlog is full, which
/// would pin this blocking worker for as long as the app process stays
/// wedged. Bounded here instead; the pending connect thread exits on its own
/// when the kernel finally answers it and drops the socket.
fn connect_bounded(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let target = path.to_path_buf();
    std::thread::Builder::new()
        .name("shenko-mobile-bridge-connect".into())
        .spawn(move || {
            let _ = sender.send(UnixStream::connect(&target));
        })
        .map_err(|error| std::io::Error::other(format!("connect thread: {error}")))?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no connection accepted within {timeout:?}"),
        )),
    }
}

impl DeviceBridge for SocketBridge {
    fn roundtrip(&self, op: BridgeOp) -> Result<serde_json::Value, BridgeError> {
        let request_id = request_id();
        let request = BridgeRequest {
            version: BRIDGE_PROTOCOL_VERSION,
            request_id: &request_id,
            operation: &op,
        };
        let mut request_bytes = serde_json::to_vec(&request).map_err(|e| BridgeError {
            kind: BridgeErrorKind::Internal,
            message: e.to_string(),
            retryable: false,
        })?;
        if request_bytes.len() > MAX_REQUEST_BYTES {
            return Err(BridgeError {
                kind: BridgeErrorKind::Unsupported,
                message: format!("request exceeds {} bytes", MAX_REQUEST_BYTES),
                retryable: false,
            });
        }
        let token = std::fs::read_to_string(&self.token_path)
            .map_err(|e| {
                Self::io_unavailable(format!(
                    "device bridge token unreadable at {}: {e}; the on-device bridge service is not running",
                    self.token_path.display()
                ))
            })?;
        let token = token.trim().to_string();
        if token.is_empty() || token.len() > 256 {
            return Err(BridgeError::typed_unavailable(
                "device bridge token malformed",
            ));
        }

        let stream = match connect_bounded(&self.socket_path, CONNECT_TIMEOUT) {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                return Err(BridgeError {
                    kind: BridgeErrorKind::Timeout,
                    message: format!(
                        "device bridge at {} accepted no connection within {CONNECT_TIMEOUT:?}; \
                         the app process may be wedged (no request was sent)",
                        self.socket_path.display()
                    ),
                    retryable: true,
                })
            }
            Err(error) => {
                return Err(Self::io_unavailable(format!(
                    "device bridge at {} unreachable: {error}; mobile plugins execute on-device \
                     through the app-owned bridge service and cannot borrow another machine's OS grants",
                    self.socket_path.display()
                )))
            }
        };
        let deadline = Self::op_timeout(&op);
        stream
            .set_read_timeout(Some(deadline))
            .map_err(|e| BridgeError::io(format!("set_read_timeout: {e}")))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| BridgeError::io(format!("set_write_timeout: {e}")))?;
        let mut stream = stream;

        // Handshake: protocol version + incarnation token. The token
        // authenticates the bridge service incarnation; it is not a grant.
        let handshake = serde_json::json!({
            "version": BRIDGE_PROTOCOL_VERSION,
            "token": token,
        });
        let mut handshake = serde_json::to_vec(&handshake).map_err(|e| BridgeError {
            kind: BridgeErrorKind::Internal,
            message: e.to_string(),
            retryable: false,
        })?;
        handshake.push(b'\n');
        stream
            .write_all(&handshake)
            .map_err(|e| BridgeError::io(format!("handshake write: {e}")))?;

        let mut reader = BufReader::with_capacity(
            64 * 1024,
            stream
                .try_clone()
                .map_err(|error| BridgeError::io(format!("stream clone: {error}")))?,
        );
        let line = match read_frame(&mut reader, MAX_HANDSHAKE_BYTES) {
            Ok(line) => line,
            Err(FrameError::Closed) => {
                return Err(BridgeError::io("bridge closed during handshake"))
            }
            Err(FrameError::Oversized { cap }) => {
                return Err(BridgeError {
                    kind: BridgeErrorKind::Unsupported,
                    message: format!("bridge handshake exceeds {cap} bytes"),
                    retryable: false,
                })
            }
            Err(FrameError::Malformed) => {
                return Err(BridgeError {
                    kind: BridgeErrorKind::Denied,
                    message: "bridge handshake is not UTF-8".to_string(),
                    retryable: false,
                })
            }
            Err(FrameError::Io(error)) => {
                return Err(BridgeError::io(format!("handshake read: {error}")))
            }
        };
        let ack: serde_json::Value = serde_json::from_str(&line).map_err(|error| BridgeError {
            kind: BridgeErrorKind::Internal,
            message: format!("handshake parse: {error}"),
            retryable: false,
        })?;
        if ack.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(BridgeError {
                kind: BridgeErrorKind::Denied,
                message: format!("bridge handshake refused: {ack}"),
                retryable: false,
            });
        }

        request_bytes.push(b'\n');
        reader
            .get_ref()
            .write_all(&request_bytes)
            .map_err(|e| BridgeError::io(format!("request write: {e}")))?;

        let response_line = match read_frame(&mut reader, MAX_RESPONSE_BYTES) {
            Ok(line) => line,
            Err(FrameError::Closed) => {
                return Err(BridgeError::io("bridge closed without a response"))
            }
            Err(FrameError::Oversized { cap }) => {
                return Err(BridgeError {
                    kind: BridgeErrorKind::Unsupported,
                    message: format!("bridge response exceeds {cap} bytes"),
                    retryable: false,
                })
            }
            Err(FrameError::Malformed) => {
                return Err(BridgeError {
                    kind: BridgeErrorKind::Internal,
                    message: "bridge response is not UTF-8".to_string(),
                    retryable: false,
                })
            }
            Err(FrameError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(BridgeError {
                    kind: BridgeErrorKind::Timeout,
                    message: format!(
                        "no bridge response within {deadline:?}; the request was sent, so the \
                         device-side effect stays unknown until observed"
                    ),
                    retryable: true,
                })
            }
            Err(FrameError::Io(error)) => {
                return Err(BridgeError::io(format!("response read: {error}")))
            }
        };
        let response: serde_json::Value =
            serde_json::from_str(&response_line).map_err(|error| BridgeError {
                kind: BridgeErrorKind::Internal,
                message: format!("response parse: {error}"),
                retryable: false,
            })?;
        if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let kind = response
                .pointer("/error/kind")
                .and_then(|v| v.as_str())
                .unwrap_or("internal");
            let message = response
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("bridge error without message")
                .to_string();
            let retryable = response
                .pointer("/error/retryable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let kind = match kind {
                "typed_unavailable" => BridgeErrorKind::TypedUnavailable,
                "denied" => BridgeErrorKind::Denied,
                "io" => BridgeErrorKind::Io,
                "unsupported" => BridgeErrorKind::Unsupported,
                "timeout" => BridgeErrorKind::Timeout,
                _ => BridgeErrorKind::Internal,
            };
            return Err(BridgeError {
                kind,
                message,
                retryable,
            });
        }
        // BridgeOps returns its successful fields alongside `ok`, not in a
        // second `result` envelope. Preserve the device's original response.
        Ok(response)
    }
}

/// Honest off-device answer: every op names the exact missing mechanism.
#[derive(Debug, Default)]
pub struct UnavailableBridge {
    pub detail: String,
}

impl UnavailableBridge {
    pub fn new() -> Self {
        Self {
            detail: "this node is not an Android device hosting the Shenko APK; \
                     mobile plugins execute on-device through the app-owned bridge \
                     and cannot borrow another machine's OS grants"
                .to_string(),
        }
    }
}

impl DeviceBridge for UnavailableBridge {
    fn roundtrip(&self, _op: BridgeOp) -> Result<serde_json::Value, BridgeError> {
        Err(BridgeError::typed_unavailable(self.detail.clone()))
    }
}

fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:016x}-{:016x}-{:08x}",
        nanos,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[allow(dead_code)]
fn assert_bridge_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<Arc<dyn DeviceBridge>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn exchange(op: BridgeOp, response: serde_json::Value) -> (Result<serde_json::Value, BridgeError>, serde_json::Value) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("bridge-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("device_bridge.token"), "synthetic-bridge-incarnation").unwrap();
        let server = UnixListener::bind(dir.join("device_bridge.sock")).unwrap();
        let worker = std::thread::spawn(move || {
            let (socket, _) = server.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let handshake: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(handshake["version"], BRIDGE_PROTOCOL_VERSION);
            assert_eq!(handshake["token"], "synthetic-bridge-incarnation");
            reader.get_mut().write_all(b"{\"ok\":true}\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            // DeviceBridgeServer forwards this object to BridgeOps.dispatch,
            // whose discriminator is a top-level string, not an object.
            let response = if request["op"].is_string() {
                response
            } else {
                serde_json::json!({"ok":false,"error":{"kind":"unsupported","message":"unknown op","retryable":false}})
            };
            serde_json::to_writer(reader.get_mut(), &response).unwrap();
            reader.get_mut().write_all(b"\n").unwrap();
            request
        });
        let result = SocketBridge::new(&dir).roundtrip(op);
        let request = worker.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        (result, request)
    }

    #[test]
    fn socket_probe_preserves_flat_device_capability_response() {
        let response = serde_json::json!({"ok":true,"binding":{"digest":"native-binding","method":"app_install_id"},"authorities":{"normal_perm":{"state":"available"}}});
        let (result, request) = exchange(BridgeOp::Probe, response.clone());
        assert_eq!(request["op"], "probe");
        assert_eq!(request["version"], BRIDGE_PROTOCOL_VERSION);
        assert!(request["request_id"].is_string());
        assert_eq!(result.unwrap(), response);
    }

    #[test]
    fn socket_shell_preserves_argv_and_flat_completion() {
        let argv = vec!["printf".into(), "中文 café \"quoted\"; $literal".into()];
        let response = serde_json::json!({"ok":true,"exit_code":7,"stdout_b64":"eA==","stderr_b64":"eQ==","timed_out":false,"authority_used":"shizuku"});
        let (result, request) = exchange(BridgeOp::ShellExec {
            authority: BridgeAuthority::Shizuku, argv: argv.clone(), env: vec!["LANG=C.UTF-8".into()], timeout_secs: 9,
        }, response.clone());
        assert_eq!(request["op"], "shell_exec");
        assert_eq!(request["authority"], "shizuku");
        assert_eq!(request["argv"], serde_json::json!(argv));
        assert_eq!(request["timeout_secs"], 9);
        assert_eq!(result.unwrap(), response);
    }

    #[test]
    fn socket_device_refusal_keeps_its_authority_and_retry_semantics() {
        let response = serde_json::json!({"ok":false,"error":{"kind":"typed_unavailable","message":"shizuku not granted","retryable":false}});
        let (result, _) = exchange(BridgeOp::CapabilityList, response);
        let error = result.unwrap_err();
        assert_eq!(error.kind, BridgeErrorKind::TypedUnavailable);
        assert_eq!(error.message, "shizuku not granted");
        assert!(!error.retryable);
    }

    /// The fingerprint request must reach the device with the exact scope this
    /// node declared — one flat `op` discriminator plus its sibling arguments —
    /// and the device's flat answer is preserved verbatim.
    #[test]
    fn socket_node_tree_fingerprint_carries_the_declared_scope() {
        let response = serde_json::json!({
            "ok": true,
            "fingerprint": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "algorithm": crate::fingerprint::FINGERPRINT_ALGORITHM,
            "display_id": 2,
            "region": [0, 0, 64, 48],
            "node_filter": "bounds_intersect_region",
            "max_nodes": 4000,
            "max_depth": 64,
            "max_windows": 8,
            "windows_considered": 1,
            "window_packages": ["com.example.app"],
            "visited_nodes": 3,
            "node_count": 3,
            "truncated": false,
            "sampled_at_ms": 1700000000000i64,
            "elapsed_ms": 2,
            "notes": [],
        });
        let (result, request) = exchange(
            BridgeOp::NodeTreeFingerprint {
                display_id: 2,
                region: [0, 0, 64, 48],
                max_nodes: 4000,
                max_depth: 64,
                max_windows: 8,
            },
            response.clone(),
        );
        assert_eq!(request["op"], "node_tree_fingerprint");
        assert_eq!(request["display_id"], 2);
        assert_eq!(request["region"], serde_json::json!([0, 0, 64, 48]));
        assert_eq!(request["max_nodes"], 4000);
        assert_eq!(request["max_depth"], 64);
        assert_eq!(request["max_windows"], 8);
        assert_eq!(result.unwrap(), response);
    }

    /// A device answer whose frames never carry LF, or carry more than the
    /// agreed bound, must not make the node buffer without limit: the bound
    /// is enforced while reading, not after the frame is already in memory.
    #[test]
    fn frame_reader_enforces_the_byte_bound_while_reading() {
        let mut exact_bytes = vec![b'a'; 16];
        exact_bytes.push(b'\n');
        assert_eq!(
            read_frame(&mut std::io::Cursor::new(exact_bytes), 16).unwrap(),
            "a".repeat(16)
        );

        let mut oversized_bytes = vec![b'a'; 17];
        oversized_bytes.push(b'\n');
        assert!(matches!(
            read_frame(&mut std::io::Cursor::new(oversized_bytes), 16),
            Err(FrameError::Oversized { cap: 16 })
        ));

        // No LF at all: rejected as soon as the bound is crossed.
        assert!(matches!(
            read_frame(&mut std::io::Cursor::new(vec![b'a'; 4096]), 64),
            Err(FrameError::Oversized { cap: 64 })
        ));
    }

    #[test]
    fn frame_reader_rejects_malformed_utf8_and_partial_frames() {
        assert!(matches!(
            read_frame(&mut std::io::Cursor::new(vec![0xff, 0xfe, b'\n']), 16),
            Err(FrameError::Malformed)
        ));
        assert!(matches!(
            read_frame(&mut std::io::Cursor::new(b"{\"ok\":true}".to_vec()), 16),
            Err(FrameError::Closed)
        ));
    }

    /// One `fill_buf` byte at a time: a code point split across reads must not
    /// change the decoded text.
    #[test]
    fn frame_reader_decodes_only_complete_multibyte_frames() {
        struct ByteAtATime {
            bytes: Vec<u8>,
            position: usize,
        }

        impl std::io::Read for ByteAtATime {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let available = self.fill_buf()?;
                let length = available.len().min(buffer.len());
                buffer[..length].copy_from_slice(&available[..length]);
                self.consume(length);
                Ok(length)
            }
        }

        impl BufRead for ByteAtATime {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                if self.position >= self.bytes.len() {
                    Ok(&[])
                } else {
                    Ok(&self.bytes[self.position..self.position + 1])
                }
            }

            fn consume(&mut self, amount: usize) {
                self.position += amount;
            }
        }

        let payload = "中文 café";
        let mut bytes = payload.as_bytes().to_vec();
        bytes.push(b'\n');
        let mut reader = ByteAtATime {
            bytes,
            position: 0,
        };
        assert_eq!(read_frame(&mut reader, 64).unwrap(), payload);
    }
}

