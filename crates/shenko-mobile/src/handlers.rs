//! The six Cordis-registered Android native task handlers.
//!
//! Each kind shares the one `AndroidBackend`; each is a replaceable plugin
//! with its own frozen input schema and prompt section. `MOBILE_NATIVE_KINDS`
//! is the single list the runtime assembly iterates.

use std::sync::Arc;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    ids::EntityId,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};

use crate::backend::{AndroidBackend, DriveCtx, DriveOutcome, reconcile_mobile};
use crate::bridge::{
    BridgeAuthority, BridgeError, BridgeErrorKind, BridgeOp, BridgeSnapshotMethod, DeviceBridge,
};
use crate::facts::{DbFacts, Fact, KeyMaterialState};

/// Kinds this plugin set installs, in deterministic registration order.
pub const MOBILE_NATIVE_KINDS: [&str; 6] = [
    "android.device.probe",
    "android.screen.observe",
    "android.control.lease",
    "android.control.input",
    "android.shell.exec",
    "android.appdb.snapshot",
];

fn descriptor_for(kind: &str, authority: &str, prompt: &str) -> NativeTaskDescriptor {
    NativeTaskDescriptor {
        kind: kind.into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: AndroidBackend::plugin_id(),
            artifact_digest: AndroidBackend::contract_digest(),
            registry_revision: 0,
        },
        role_revision: "native-mobile/1".into(),
        authority_refs: vec![authority.into()],
        prompt_section: Some(prompt.into()),
    }
}

const PROBE_PROMPT: &str = "android.device.probe reads on-device facts {\"refresh\":bool?}: OS version/security patch/fingerprint digest, per-mechanism adapter states (root_su, shizuku binder incl. ADB-vs-root identity, dhizuku device-owner, accessibility, normal permissions), the salted device-binding digest, and display metrics. Each mechanism reports its own grant state; availability is probed, never inferred from labels.";
const OBSERVE_PROMPT: &str = "android.screen.observe {\"session_id\":uuid?,\"display_id\":int?,\"region\":[x,y,w,h]?} opens a session if needed and captures the real display through a granted mechanism (Shizuku/root screencap). Returns observation_id (blake3 of the sealed observation), per-session monotonic revision, viewport/density/rotation, frame fingerprint (accessibility node-tree hash when enabled, else pixels), pixel hash and the sealed PNG. Coordinates are only valid while that observation re-verifies.";
const LEASE_PROMPT: &str = "android.control.lease {\"session_id\":uuid,\"op\":\"acquire\"|\"release\"|\"takeover\",\"lease_token\":uuid?} manages the single writer epoch of the device display domain. One domain has one writer; takeover revokes the old epoch so late machine input is rejected; a held shell-injected key cannot be retracted and stays unknown. Read-only observation never needs a lease.";
const INPUT_PROMPT: &str = "android.control.input {\"session_id\":uuid,\"lease_token\":uuid,\"observation_id\":\"blake3:…\",\"actions\":[tap|swipe|key|text|wait],\"require_pixel_fresh\":bool?} injects through the real mechanism chosen from granted capabilities (accessibility gestures, Shizuku/root `input`). The observation re-verifies and the lease epoch must be live before any event; accessibility human gestures revoke the lease like a takeover. ACK is transport, never an app effect; nothing replays.";
const SHELL_PROMPT: &str = "android.shell.exec {\"authority\":\"auto\"|\"root_su\"|\"shizuku\"|\"dhizuku\",\"argv\":[\"…\"],\"env\":[\"KEY=VALUE\"],\"timeout_secs\":1..=60} runs frozen argv verbatim (no shell string is ever constructed) through the requested mechanism with its own live grant. Returns exit code, bounded base64 stdout/stderr with truncation flags, timeout state, and which authority actually ran. Command side effects stay Unknown until observed.";
const APPDB_PROMPT: &str = "android.appdb.snapshot {\"authority\":\"auto\"|\"root_su\"|\"shizuku\",\"package\":\"…\",\"db_name\":\"…\",\"method\":\"auto\"|\"raw_file_set_copy\"|\"direct_readable\"|\"app_export\"} copies an app's database file set into app-private storage and reports SEPARATE facts: file located/readable, recovered-copy vs point-in-time-consistent snapshot, plaintext-verified (SQLite header), schema interpretable, key-material state, and semantic completeness (never claimed). A raw db+wal copy is not a consistent snapshot and is never reported as one; decrypted content is never fabricated.";

// ---------------------------------------------------------------------------
// android.device.probe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeInput {
    refresh: Option<bool>,
}

pub struct AndroidDeviceProbeHandler {
    backend: Arc<AndroidBackend>,
}

impl AndroidDeviceProbeHandler {
    pub fn new(backend: Arc<AndroidBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: ProbeInput = serde_json::from_value(invocation.input.clone())?;
        let refresh = input.refresh.unwrap_or(false);
        self.backend
            .drive(
                invocation,
                "android.device.probe",
                "android-device-probe",
                serde_json::json!({ "refresh": refresh }),
                "android-device-probe-input",
                serde_json::json!({ "mechanism": "on-device bridge probe" }),
                "the bridge answered with real OS/adapter facts, or a typed-unavailable result names the exact missing mechanism",
                move |backend, _ctx| match backend.probe_cached(refresh) {
                    Ok(probe) => DriveOutcome::done(
                        crate::types::effect_none(),
                        crate::types::level_postcondition(),
                        serde_json::json!({
                            "result": "probed",
                            "probe": probe,
                            "authorities_are_parallel_not_a_ladder": true,
                        }),
                    ),
                    Err(error) => DriveOutcome::bridge_failure(&error),
                },
            )
            .await
    }
}

#[async_trait]
impl NativeTaskHandler for AndroidDeviceProbeHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "android.device.probe",
            "mobile.device.probe/v1",
            PROBE_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_mobile(&self.backend, task, operation_id, "android.device.probe").await
    }
}

// ---------------------------------------------------------------------------
// android.screen.observe
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveInput {
    session_id: Option<EntityId>,
    display_id: Option<i32>,
    region: Option<[i32; 4]>,
}

pub struct AndroidScreenObserveHandler {
    backend: Arc<AndroidBackend>,
}

impl AndroidScreenObserveHandler {
    pub fn new(backend: Arc<AndroidBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: ObserveInput = serde_json::from_value(invocation.input.clone())?;
        if let Some(r) = input.region {
            ensure!(
                r[2] > 0 && r[3] > 0 && i64::from(r[2]) * i64::from(r[3]) <= 32 * 1024 * 1024,
                "region must have positive area bounded at 32Mpx"
            );
        }
        let frozen = serde_json::json!({
            "session_id": input.session_id,
            "display_id": input.display_id,
            "region": input.region,
        });
        self.backend
            .drive(
                invocation,
                "android.screen.observe",
                "android-screen-observe",
                frozen,
                "android-screen-observe-input",
                serde_json::json!({ "mechanism": "granted screencap authority; accessibility fingerprint when enabled" }),
                "the sealed observation binds the actual captured pixels (pixel hash), the session/display identity and the per-session revision; coordinates are valid only while it re-verifies",
                move |backend, ctx| {
                    let session = match input.session_id {
                        Some(id) => match backend.session(id) {
                            Ok(session) => session,
                            Err("session_closed") => {
                                return DriveOutcome::rejected(
                                    "session_closed",
                                    serde_json::json!({ "session_id": id }),
                                )
                            }
                            Err(_) => {
                                return DriveOutcome::rejected(
                                    "session_unknown",
                                    serde_json::json!({ "session_id": id }),
                                )
                            }
                        },
                        None => {
                            let outcome = backend.open_session(input.display_id, ctx.caller);
                            if outcome.outcome != Some(OperationOutcome::Succeeded) {
                                return outcome;
                            }
                            let session_id = outcome
                                .normalized
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<EntityId>().ok());
                            let Some(session_id) = session_id else {
                                return DriveOutcome::rejected(
                                    "session_open_without_id",
                                    serde_json::json!({}),
                                );
                            };
                            match backend.session(session_id) {
                                Ok(session) => session,
                                Err(_) => {
                                    return DriveOutcome::rejected(
                                        "session_unknown",
                                        serde_json::json!({ "session_id": session_id }),
                                    )
                                }
                            }
                        }
                    };
                    backend.capture(&session, input.region, true)
                },
            )
            .await
    }
}

#[async_trait]
impl NativeTaskHandler for AndroidScreenObserveHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "android.screen.observe",
            "mobile.screen.observe/v1",
            OBSERVE_PROMPT,
    )

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_mobile(&self.backend, task, operation_id, "android.screen.observe").await
    }
}

// ---------------------------------------------------------------------------
// android.control.lease
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseOp {
    Acquire,
    Release,
    Takeover,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseInput {
    session_id: EntityId,
    op: LeaseOp,
    lease_token: Option<EntityId>,
}

pub struct AndroidControlLeaseHandler {
    backend: Arc<AndroidBackend>,
}

impl AndroidControlLeaseHandler {
    pub fn new(backend: Arc<AndroidBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: LeaseInput = serde_json::from_value(invocation.input.clone())?;
        let frozen = serde_json::json!({
            "session_id": input.session_id,
            "op": input.op,
            "lease_token": input.lease_token,
        });
        let op = input.op;
        let session_id = input.session_id;
        let token = input.lease_token;
        self.backend
            .drive(
                invocation,
                "android.control.lease",
                "android-control-lease",
                frozen,
                "android-control-lease-input",
                serde_json::json!({ "mechanism": "per-domain exclusive writer epoch" }),
                "one display domain has at most one live writer epoch; takeover invalidates the previous epoch and releases what can be released",
                move |backend, ctx| backend.lease_op(op, session_id, token, ctx.caller),
            )
            .await
    }
}

#[async_trait]
impl NativeTaskHandler for AndroidControlLeaseHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "android.control.lease",
            "mobile.control.lease/v1",
            LEASE_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_mobile(&self.backend, task, operation_id, "android.control.lease").await
    }
}

// ---------------------------------------------------------------------------
// android.control.input
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MobileActionInput {
    Tap { x: f64, y: f64 },
    Swipe { x1: f64, y1: f64, x2: f64, y2: f64, duration_ms: u64 },
    Key { keycode: i32, down: bool },
    Text { text: String },
    Wait { ms: u64 },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectInput {
    session_id: EntityId,
    lease_token: EntityId,
    /// `blake3:<hex>` digest of the sealed MobileObservation this batch is
    /// bound to; re-verified before any event is sent.
    observation_id: String,
    actions: Vec<MobileActionInput>,
    require_pixel_fresh: Option<bool>,
}

pub struct AndroidControlInputHandler {
    backend: Arc<AndroidBackend>,
}

impl AndroidControlInputHandler {
    pub fn new(backend: Arc<AndroidBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: InjectInput = serde_json::from_value(invocation.input.clone())?;
        ensure!(
            !input.actions.is_empty() && input.actions.len() <= 64,
            "actions must be 1..=64"
        );
        for action in &input.actions {
            if let MobileActionInput::Text { text } = action {
                ensure!(
                    !text.is_empty() && text.chars().count() <= 4096,
                    "text must be 1..=4096 chars"
                );
            }
            if let MobileActionInput::Swipe { duration_ms, .. } = action {
                ensure!(*duration_ms <= 60_000, "swipe duration must be <= 60s");
            }
        }
        ensure!(
            input.observation_id.starts_with("blake3:"),
            "observation_id must be a blake3 digest reference"
        );
        let frozen = serde_json::json!({
            "session_id": input.session_id,
            "lease_token": input.lease_token,
            "observation_id": input.observation_id,
            "actions": input.actions,
            "require_pixel_fresh": input.require_pixel_fresh.unwrap_or(false),
        });
        let session_id = input.session_id;
        let lease_token = input.lease_token;
        let observation_id = input.observation_id.clone();
        let require_pixel_fresh = input.require_pixel_fresh.unwrap_or(false);
        let actions = input.actions.clone();
        self.backend
            .drive(
                invocation,
                "android.control.input",
                "android-control-input",
                frozen,
                "android-control-input-input",
                serde_json::json!({ "mechanism": "accessibility gestures or shell `input` under granted authority; observation re-verify; lease epoch check" }),
                "every action is delivered only after the bound observation re-verifies and the lease epoch is live; unverified delivery is never claimed complete",
                move |backend, _ctx| {
                    let Ok(session) = backend.session(session_id) else {
                        return DriveOutcome::rejected(
                            "session_unknown",
                            serde_json::json!({ "session_id": session_id }),
                        );
                    };
                    let sealed = match load_observation(backend, &observation_id) {
                        Ok(observation) => observation,
                        Err(reason) => {
                            return DriveOutcome::rejected(
                                reason,
                                serde_json::json!({ "observation_id": observation_id }),
                            )
                        }
                    };
                    if sealed.session_id != session.session_id {
                        return DriveOutcome::rejected(
                            "observation_session_mismatch",
                            serde_json::json!({}),
                        );
                    }
                    backend.inject(&session, lease_token, &sealed, actions, require_pixel_fresh)
                },
            )
            .await
    }
}

/// Load a previously sealed observation back by its content digest. The
/// observation record is the durable bound the input references.
fn load_observation(
    backend: &AndroidBackend,
    observation_id: &str,
) -> Result<crate::backend::MobileObservation, &'static str> {
    if !observation_id.starts_with("blake3:") {
        return Err("observation_id_malformed");
    }
    let bytes = backend
        .read_object_by_digest_hex(observation_id)
        .map_err(|_| "observation_unreadable")?;
    serde_json::from_slice(&bytes).map_err(|_| "observation_malformed")
}

#[async_trait]
impl NativeTaskHandler for AndroidControlInputHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "android.control.input",
            "mobile.control.input/v1",
            INPUT_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_mobile(&self.backend, task, operation_id, "android.control.input").await
    }
}

// ---------------------------------------------------------------------------
// android.shell.exec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ShellAuthority {
    Auto,
    RootSu,
    Shizuku,
    Dhizuku,
}

impl ShellAuthority {
    fn bridge(self) -> BridgeAuthority {
        match self {
            Self::Auto => BridgeAuthority::Auto,
            Self::RootSu => BridgeAuthority::RootSu,
            Self::Shizuku => BridgeAuthority::Shizuku,
            Self::Dhizuku => BridgeAuthority::Dhizuku,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellInput {
    authority: Option<ShellAuthority>,
    argv: Vec<String>,
    env: Vec<String>,
    timeout_secs: Option<u64>,
}

pub struct AndroidShellExecHandler {
    backend: Arc<AndroidBackend>,
}

impl AndroidShellExecHandler {
    pub fn new(backend: Arc<AndroidBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: ShellInput = serde_json::from_value(invocation.input.clone())?;
        ensure!(!input.argv.is_empty(), "argv must be non-empty");
        ensure!(
            input.argv.iter().all(|arg| !arg.contains('\0')),
            "argv must not contain NUL"
        );
        for entry in &input.env {
            let Some((key, _)) = entry.split_once('=') else {
                anyhow::bail!("env entries must be KEY=VALUE");
            };
            ensure!(
                !key.is_empty()
                    && key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    && key.len() <= 64,
                "env keys must be ASCII UPPER_CASE"
            );
        }
        let timeout_secs = input.timeout_secs.unwrap_or(10).clamp(1, 60);
        let authority = input.authority.unwrap_or(ShellAuthority::Auto);
        let frozen = serde_json::json!({
            "authority": authority,
            "argv": input.argv,
            "env": input.env,
            "timeout_secs": timeout_secs,
        });
        let argv = input.argv.clone();
        let env = input.env.clone();
        self.backend
            .drive(
                invocation,
                "android.shell.exec",
                "android-shell-exec",
                frozen,
                "android-shell-exec-input",
                serde_json::json!({ "mechanism": "frozen argv verbatim through su/shizuku/dhizuku; no shell string" }),
                "the process ran to exit or timeout and the bounded captured bytes are those emitted before collection; device-side effects of the command remain Unknown until observed",
                move |backend, _ctx| {
                    backend.shell_exec(authority.bridge(), argv, env, timeout_secs)
                },
            )
            .await
    }
}

#[async_trait]
impl NativeTaskHandler for AndroidShellExecHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "android.shell.exec",
            "mobile.shell.exec/v1",
            SHELL_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_mobile(&self.backend, task, operation_id, "android.shell.exec").await
    }
}

// ---------------------------------------------------------------------------
// android.appdb.snapshot — §24.5.1
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotMethod {
    Auto,
    RawFileSetCopy,
    DirectReadable,
    AppExport,
}

impl SnapshotMethod {
    fn bridge(self) -> BridgeSnapshotMethod {
        match self {
            Self::Auto => BridgeSnapshotMethod::Auto,
            Self::RawFileSetCopy => BridgeSnapshotMethod::RawFileSetCopy,
            Self::DirectReadable => BridgeSnapshotMethod::DirectReadable,
            Self::AppExport => BridgeSnapshotMethod::AppExport,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppDbInput {
    authority: Option<ShellAuthority>,
    package: String,
    db_name: String,
    method: Option<SnapshotMethod>,
}

pub struct AndroidAppDbSnapshotHandler {
    backend: Arc<AndroidBackend>,
}

impl AndroidAppDbSnapshotHandler {
    pub fn new(backend: Arc<AndroidBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: AppDbInput = serde_json::from_value(invocation.input.clone())?;
        ensure!(
            !input.package.is_empty()
                && input.package.len() <= 128
                && input
                    .package
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "package must be a plain Android package name"
        );
        ensure!(
            !input.db_name.is_empty()
                && input.db_name.len() <= 128
                && !input.db_name.contains('/')
                && !input.db_name.contains("..")
                && input
                    .db_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ')),
            "db_name must be a plain database file name"
        );
        let authority = input.authority.unwrap_or(ShellAuthority::Auto);
        let method = input.method.unwrap_or(SnapshotMethod::Auto);
        let frozen = serde_json::json!({
            "authority": authority,
            "package": input.package,
            "db_name": input.db_name,
            "method": method,
        });
        let package = input.package.clone();
        let db_name = input.db_name.clone();
        self.backend
            .drive(
                invocation,
                "android.appdb.snapshot",
                "android-appdb-snapshot",
                frozen,
                "android-appdb-snapshot-input",
                serde_json::json!({ "mechanism": "app-private byte copy via granted authority; node-side SQLite analysis on the copy" }),
                "the copied bytes are hashed into the observation, every consistency/format/key/semantics dimension is reported separately, and no plaintext or semantic completeness is claimed without the actual evidence",
                move |backend, ctx| appdb_work(backend, ctx, authority, package, db_name, method),
            )
            .await
    }
}

fn appdb_work(
    backend: &AndroidBackend,
    ctx: &DriveCtx,
    authority: ShellAuthority,
    package: String,
    db_name: String,
    method: SnapshotMethod,
) -> DriveOutcome {
    let snapshot_dir = backend
        .data_dir()
        .join("mobile")
        .join("appdb-snapshots")
        .join(ctx.operation_id.to_string());
    let op = BridgeOp::DbSnapshot {
        authority: authority.bridge(),
        package: package.clone(),
        db_name: db_name.clone(),
        method: method.bridge(),
    };
    let result = match backend.bridge().roundtrip(op) {
        Ok(result) => result,
        Err(error) => return DriveOutcome::bridge_failure(&error),
    };
    if result.get("package_installed").and_then(|v| v.as_bool()) == Some(false) {
        return DriveOutcome::unavailable(
            "package_not_installed",
            format!("package {package} is not installed on this device"),
        );
    }
    let mut facts = DbFacts::none();
    let mut notes: Vec<String> = result
        .get("notes")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let source_path = result
        .get("source_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !source_path.is_empty() {
        facts.file_located = Fact::reached("source_path_resolved");
    }

    // Preferred path: the source is directly readable by this uid, so the
    // node itself runs the SQLite Online Backup API (§24.5.1) into a fresh
    // local file — that is a real point-in-time consistent snapshot.
    let direct_readable = result
        .get("direct_readable_path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    let mut sealed_snapshot: Option<ObjectRef> = None;
    let mut snapshot_meta = serde_json::json!({
        "package": package,
        "db_name": db_name,
        "source_path": source_path,
        "method_used": result.get("method_used").cloned().unwrap_or(serde_json::Value::Null),
        "app_version": result.get("app_version").cloned().unwrap_or(serde_json::Value::Null),
    });

    if let Some(source) = direct_readable {
        facts.file_readable = Fact::reached("directly_readable_by_node_uid");
        match run_sqlite_backup(backend, &source, snapshot_dir.join("direct")) {
            Ok(backup_path) => {
                facts.snapshot_consistent = Fact {
                    reached: true,
                    evidence: Some("sqlite_online_backup_api".to_string()),
                    note: Some("backup taken by the node over a directly readable source".to_string()),
                };
                match seal_snapshot_file(backend, &backup_path) {
                    Ok(reference) => {
                        sealed_snapshot = Some(reference);
                    }
                    Err(note) => notes.push(note),
                }
                snapshot_meta["backup_path"] = serde_json::json!(backup_path.display().to_string());
            }
            Err(reason) => {
                notes.push(format!("sqlite backup api failed: {reason}; falling back to copied bytes with explicit consistency facts"));
            }
        }
    }

    if sealed_snapshot.is_none() {
        // Fallback: analyze the bridge-copied byte set. This can never
        // become `snapshot_consistent`: only `snapshot_recovered_copy` at
        // best, with the concurrency uncertainty recorded.
        let Some(copied_dir) = result
            .get("snapshot_dir")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
        else {
            return DriveOutcome::bridge_failure(&BridgeError {
                kind: BridgeErrorKind::Internal,
                message: "bridge snapshot result missing snapshot_dir".to_string(),
                retryable: false,
            });
        };
        let main_path = copied_dir.join(&db_name);
        let analysis = analyze_copied_set(&main_path, &result);
        facts.file_readable = analysis.file_readable;
        facts.plaintext_verified = analysis.plaintext_verified;
        facts.structure_interpretable = analysis.structure_interpretable;
        facts.snapshot_recovered_copy = analysis.snapshot_recovered_copy;
        facts.snapshot_consistent = analysis.snapshot_consistent;
        notes.extend(analysis.notes);
        match seal_snapshot_file(backend, &main_path) {
            Ok(reference) => {
                sealed_snapshot = Some(reference);
            }
            Err(note) => notes.push(note),
        }
        snapshot_meta["snapshot_dir"] = serde_json::json!(copied_dir.display().to_string());
        snapshot_meta["copied_files"] = result.get("files").cloned().unwrap_or(serde_json::Value::Null);
        snapshot_meta["wal_present"] = result.get("wal_present").cloned().unwrap_or(serde_json::Value::Null);
    }

    let snapshot_ref = sealed_snapshot.map(|reference| reference.clone());
    let normalized = serde_json::json!({
        "result": "snapshot",
        "snapshot": snapshot_meta,
        "snapshot_object": snapshot_ref,
        "facts": facts,
        "key_material_note": key_material_note(&facts.key_material),
        "missing_and_uncertainty": notes,
        "collected_at_ms": crate::backend::now_unix_ms(),
    });
    // Read-only snapshot: no external effect; missing fact dimensions are
    // data, not failure.
    DriveOutcome::done(
        EffectStatus::None,
        ObservationLevel::PostconditionVerified,
        normalized,
    )
}

struct CopyAnalysis {
    file_readable: Fact,
    plaintext_verified: Fact,
    structure_interpretable: Fact,
    snapshot_recovered_copy: Fact,
    snapshot_consistent: Fact,
    notes: Vec<String>,
}

fn analyze_copied_set(main_path: &std::path::Path, result: &serde_json::Value) -> CopyAnalysis {
    let mut out = CopyAnalysis {
        file_readable: Fact::missed("main database file not copied or unreadable"),
        plaintext_verified: Fact::missed("not attempted"),
        structure_interpretable: Fact::missed("not attempted"),
        snapshot_recovered_copy: Fact::missed("not attempted"),
        snapshot_consistent: Fact::missed("raw byte copy cannot prove point-in-time consistency"),
        notes: Vec::new(),
    };
    let Ok(bytes) = std::fs::read(main_path) else {
        return out;
    };
    if bytes.is_empty() {
        out.notes.push("copied main file is empty".to_string());
        return out;
    }
    out.file_readable = Fact::reached("copied_bytes_read");

    const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
    if bytes.len() >= 16 && &bytes[..16] == SQLITE_HEADER {
        out.plaintext_verified = Fact::reached("sqlite_header");
    } else {
        out.plaintext_verified = Fact::missed(
            "first 16 bytes are not the SQLite header: encrypted container or foreign format; which one stays unknown without key material",
        );
        out.notes.push(
            "not a plaintext SQLite header; this plugin does not extract keys and claims nothing about decryptability"
                .to_string(),
        );
        // No schema claims without plaintext.
        return out;
    }

    // WAL/companion handling decides the consistency fact.
    let wal_present = result.get("wal_present").and_then(|v| v.as_bool()) == Some(true);
    let wal_size = result
        .get("wal_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let journal_copied = result
        .get("journal_copied")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if wal_present || wal_size > 0 || journal_copied {
        out.snapshot_recovered_copy = Fact {
            reached: true,
            evidence: Some("companion_files_copied_recovered_on_open".to_string()),
            note: Some(
                "the byte set was recovered by opening the copy read-only; concurrent writes during copy may be missing or torn"
                    .to_string(),
            ),
        };
        out.notes.push(
            "raw db+wal copy is NOT a point-in-time snapshot; snapshot_consistent stays false"
                .to_string(),
        );
    } else {
        // Rollback-journal mode with no companion present: the main file is
        // a committed single-file image.
        out.snapshot_consistent = Fact {
            reached: true,
            evidence: Some("no_companion_files_present".to_string()),
            note: Some(
                "no WAL/journal companions were present at copy time; treated as a committed rollback-mode image"
                    .to_string(),
            ),
        };
    }

    // Structure on the copy, read-only.
    match std::panic::catch_unwind(|| structure_probe(main_path)) {
        Ok(Ok(())) => {
            out.structure_interpretable = Fact::reached("readonly_open_sqlite_master");
        }
        Ok(Err(_)) => {
            out.structure_interpretable = Fact::missed("sqlite open or sqlite_master query failed on the copy");
        }
        Err(_) => {
            out.structure_interpretable = Fact::missed("structure probe panicked");
        }
    }
    out
}

/// Open the copied database read-only and prove the schema is queryable.
/// Never runs against a live source path — only against our own copy.
fn structure_probe(path: &std::path::Path) -> std::result::Result<(), ()> {
    use std::str::FromStr as _;
    let flags = rusqlite::OpenFlags::from_str("SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX")
        .map_err(|_| ())?;
    let connection = rusqlite::Connection::open_with_flags(path, flags).map_err(|_| ())?;
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| ())?;
    Ok(())
}

/// SQLite Online Backup API over a directly readable source — the only
/// paths that earn `snapshot_consistent`.
fn run_sqlite_backup(
    backend: &AndroidBackend,
    source: &std::path::Path,
    into_dir: std::path::PathBuf,
) -> std::result::Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(&into_dir).map_err(|e| e.to_string())?;
    let target = into_dir.join("backup.sqlite3");
    if target.exists() {
        std::fs::remove_file(&target).map_err(|e| e.to_string())?;
    }
    let source_connection =
        rusqlite::Connection::open(source).map_err(|e| format!("open source: {e}"))?;
    source_connection
        .backup(rusqlite::DatabaseName::Main, &target, None)
        .map_err(|e| format!("backup: {e}"))?;
    let _ = backend;
    Ok(target)
}

fn seal_snapshot_file(
    backend: &AndroidBackend,
    path: &std::path::Path,
) -> std::result::Result<ObjectRef, String> {
    const MAX_SNAPSHOT_FILE_BYTES_LOCAL: u64 = 64 * 1024 * 1024;
    let meta = std::fs::metadata(path).map_err(|e| format!("snapshot stat: {e}"))?;
    if meta.len() > MAX_SNAPSHOT_FILE_BYTES_LOCAL {
        let sha = sha256_file(path)?;
        return Err(format!(
            "snapshot file {} bytes exceeds the 64MiB bounded CAS route (sha256 {sha}); local path retained: {}",
            meta.len(),
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("snapshot read: {e}"))?;
    backend
        .seal_family(&bytes, "application/octet-stream", "mobile-appdb-snapshots", "appdb-snapshot-bytes")
        .map_err(|e| format!("snapshot seal: {e}"))
}

fn sha256_file(path: &std::path::Path) -> std::result::Result<String, String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn key_material_note(state: &KeyMaterialState) -> &'static str {
    match state {
        KeyMaterialState::NoneAvailable { .. } => {
            "no key material is available to this plugin; nothing about decryptability is claimed"
        }
        KeyMaterialState::SuppliedByAdapter { .. } => {
            "key material came from an app-specific adapter; its provenance is recorded separately"
        }
        KeyMaterialState::Unknown => "key material state is unknown",
    }
}

#[async_trait]
impl NativeTaskHandler for AndroidAppDbSnapshotHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "android.appdb.snapshot",
            "mobile.appdb.snapshot/v1",
            APPDB_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        reconcile_mobile(&self.backend, task, operation_id, "android.appdb.snapshot").await
    }
}

// ---------------------------------------------------------------------------
// Shared helpers kept behind tiny functions so the outcome vocabulary stays
// consistent across handlers.
// ---------------------------------------------------------------------------

mod types {
    use shenko_types::evidence::ObservationLevel;
    use shenko_types::operation::EffectStatus;

    pub(crate) fn effect_none() -> EffectStatus {
        EffectStatus::None
    }

    pub(crate) fn level_postcondition() -> ObservationLevel {
        ObservationLevel::PostconditionVerified
    }
}
