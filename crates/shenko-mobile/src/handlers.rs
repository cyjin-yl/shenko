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
    evidence::ObservationLevel,
    ids::EntityId,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};

use crate::backend::{AndroidBackend, DriveCtx, DriveOutcome, reconcile_mobile};
use crate::bridge::{
    BridgeAuthority, BridgeOp, BridgeSnapshotMethod, DeviceBridge,
};
use crate::facts::{DbSnapshot, KeyMaterialState, SnapshotDimension};

/// Kinds this plugin set installs, in deterministic registration order.
pub const MOBILE_NATIVE_KINDS: [&str; 6] = [
    "android.device.probe",
    "android.screen.observe",
    "android.control.lease",
    "android.control.input",
    "android.shell.exec",
    "android.appdb.snapshot",
];

/// Input bounds, validated before any event can leave the process.
const MAX_ACTIONS: usize = 64;
const MAX_TEXT_CHARS: usize = 4096;
/// The device sleeps through waits and swipe durations on one bridge
/// connection, so the whole batch is bounded rather than each action alone.
const MAX_BATCH_MS: u64 = 120_000;

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
const OBSERVE_PROMPT: &str = "android.screen.observe {\"session_id\":uuid?,\"display_id\":int?,\"region\":[x,y,w,h]?} opens a session if needed and captures the real display through a granted mechanism (Shizuku/root screencap), then asks the enabled accessibility service to fingerprint the node hierarchy it can actually see in that same rectangle. Returns observation_id (blake3 of the sealed observation), per-session monotonic revision, viewport/density/rotation, the pixel hash and the sealed PNG, plus a frame fingerprint with its honest kind: accessibility_node_tree (device-attested digest of every node of every reported window, complete within the declared traversal bounds), accessibility_node_tree_prefix (the traversal stopped at a declared node/depth/window bound, so the digest covers only that declared prefix), or pixels_only (no hierarchy evidence: fingerprint_absent records why, e.g. the service is not enabled or the window content is not retrievable). The hierarchy digest travels with its display/region scope, the traversal bounds applied, the covered windows/packages and its sampling time; node text never leaves the device. Coordinates are only valid while that observation re-verifies.";
const LEASE_PROMPT: &str = "android.control.lease {\"session_id\":uuid,\"op\":\"acquire\"|\"release\"|\"takeover\"|\"close\",\"lease_token\":uuid?} manages the single writer epoch of the device display domain. One domain has one writer; takeover revokes the old epoch so late machine input is rejected, and input re-checks the epoch and the human-activity feed after it re-verifies the bound observation. These mechanisms deliver complete key presses only, so no held key or button state is ever left by this plugin. `close` ends the session (it stops accepting capture and input and releases only its own writer lease, without bumping the epoch), so a long-lived node does not accumulate observation sessions. Read-only observation never needs a lease.";
const INPUT_PROMPT: &str = "android.control.input {\"session_id\":uuid,\"lease_token\":uuid,\"observation_id\":\"blake3:…\",\"actions\":[tap|swipe|key|text|wait],\"require_pixel_fresh\":bool?} injects through the real mechanism chosen from granted capabilities (accessibility gestures, Shizuku/root `input`). Every tap and swipe point must lie inside the rectangle the bound observation actually saw, batches are bounded at 64 actions and 120s of waits/swipes, and the lease epoch and human-activity feed are checked again after the observation re-verifies and before the batch leaves, so a takeover during verification rejects the batch. Re-verification compares the evidence the observation actually bound inside the same declared scope: a re-observed hierarchy digest must match, so a semantic change with identical pixels rejects the batch (stale_observation_hierarchy), while hierarchy evidence that vanished, changed kind or came back over another display/region/bound is rejected as unverifiable (stale_observation_fingerprint_unavailable / _kind / _scope) instead of being treated as unchanged; a pixels_only observation compares pixels only and never claims the hierarchy was unchanged. A `key` action is a complete press: these mechanisms cannot hold a key down, so a key requested with down=true is rejected instead of being reported as delivered (release held modifiers yourself by prompting a press). ACK is transport, never an app effect: refused or unanswered actions are reported per index and make the batch partial; nothing replays.";
const SHELL_PROMPT: &str = "android.shell.exec {\"authority\":\"auto\"|\"root_su\"|\"shizuku\"|\"dhizuku\",\"argv\":[\"…\"],\"env\":[\"KEY=VALUE\"],\"timeout_secs\":1..=60} runs frozen argv verbatim (no shell string is ever constructed) through the requested mechanism with its own live grant. Returns exit code, bounded base64 stdout/stderr with truncation flags, timeout state, and which authority actually ran. Command side effects stay Unknown until observed.";
const APPDB_PROMPT: &str = "android.appdb.snapshot {\"authority\":\"auto\"|\"root_su\"|\"shizuku\",\"package\":\"…\",\"db_name\":\"…\",\"method\":\"auto\"|\"raw_file_set_copy\"|\"direct_readable\"|\"app_export\"} copies an app's database file set into app-private storage and reports SEPARATE snapshot dimensions (reached or not, with what proved it; these are not cognitive facts and no knowledge is asserted): file located/readable, recovered-copy vs point-in-time-consistent snapshot, plaintext-verified (SQLite header), schema interpretable, key-material state, and semantic completeness (never claimed). A raw db+wal copy is not a consistent snapshot and is never reported as one; decrypted content is never fabricated, and understanding what the rows mean stays a later subject judgment over actually recovered plaintext.";

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
                        types::effect_none(),
                        types::level_postcondition(),
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
    }

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
    /// End the session: it stops accepting capture and input, and its own
    /// writer lease (if it holds one) is released. Not a takeover, so the
    /// domain epoch is untouched for any other writer.
    Close,
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
        validate_actions(&input.actions)?;
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

/// Every rule an input batch must satisfy before any event can leave the
/// process, checked on the action list alone.
fn validate_actions(actions: &[MobileActionInput]) -> Result<()> {
    ensure!(
        !actions.is_empty() && actions.len() <= MAX_ACTIONS,
        "actions must be 1..={MAX_ACTIONS}"
    );
    let mut batch_ms: u64 = 0;
    for action in actions {
        if let MobileActionInput::Text { text } = action {
            ensure!(
                !text.is_empty() && text.chars().count() <= MAX_TEXT_CHARS,
                "text must be 1..={MAX_TEXT_CHARS} chars"
            );
            // `input text` passes the text as one argv entry; a NUL would
            // truncate it inside the device process instead of typing it.
            ensure!(!text.contains('\0'), "text must not contain NUL");
        }
        if let MobileActionInput::Swipe { duration_ms, .. } = action {
            ensure!(*duration_ms <= 60_000, "swipe duration must be <= 60s");
            batch_ms = batch_ms.saturating_add(*duration_ms);
        }
        if let MobileActionInput::Wait { ms } = action {
            batch_ms = batch_ms.saturating_add(*ms);
        }
    }
    // The device sleeps through waits and swipe durations on one bridge
    // connection; an unbounded total would hold that connection (and this
    // blocking worker) for as long as the caller asked.
    ensure!(
        batch_ms <= MAX_BATCH_MS,
        "total wait and swipe duration must be <= {MAX_BATCH_MS}ms"
    );
    Ok(())
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
    // A snapshot's dimensions, not cognitive facts (§51.9): the payload keeps
    // its historical `facts` field name so already sealed records stay
    // readable, while the Rust type is `DbSnapshot`/`SnapshotDimension`.
    let mut facts = DbSnapshot::none();
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
        facts.file_located = SnapshotDimension::reached("source_path_resolved");
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

    let mut backup_failure: Option<String> = None;
    if let Some(source) = direct_readable {
        facts.file_readable = SnapshotDimension::reached("directly_readable_by_node_uid");
        match run_sqlite_backup(&source, snapshot_dir.join("direct")) {
            Ok(backup_path) => {
                facts.snapshot_consistent = SnapshotDimension {
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
                facts.snapshot_consistent = SnapshotDimension::missed(format!(
                    "the SQLite backup API failed on the directly readable source: {reason}"
                ));
                notes.push(format!(
                    "sqlite backup api failed: {reason}; no consistent snapshot was produced"
                ));
                backup_failure = Some(reason);
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
            // No consistent backup and no copied byte set: the operation did
            // not reach a snapshot. Report the device's real reason together
            // with the facts that were established, instead of replacing
            // them with an internal "missing field" error.
            notes.push(
                "the bridge returned neither a consistent backup nor a copied file set"
                    .to_string(),
            );
            return DriveOutcome::incomplete(serde_json::json!({
                "result": "snapshot_unavailable",
                "snapshot": snapshot_meta,
                "facts": facts,
                "key_material_note": key_material_note(&facts.key_material),
                "missing_and_uncertainty": notes,
                "backup_failure": backup_failure,
                "collected_at_ms": chrono::Utc::now().timestamp_millis(),
            }));
        };
        let main_path = copied_dir.join(&db_name);
        let analysis = analyze_copied_set(&main_path, &result);
        facts.file_readable = analysis.file_readable;
        facts.plaintext_verified = analysis.plaintext_verified;
        facts.structure_interpretable = analysis.structure_interpretable;
        facts.snapshot_recovered_copy = analysis.snapshot_recovered_copy;
        // A failed backup keeps its own reason; the copy analysis cannot
        // upgrade it to consistency, only restate why it is missing.
        if backup_failure.is_none() {
            facts.snapshot_consistent = analysis.snapshot_consistent;
        }
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
        "collected_at_ms": chrono::Utc::now().timestamp_millis(),
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
    file_readable: SnapshotDimension,
    plaintext_verified: SnapshotDimension,
    structure_interpretable: SnapshotDimension,
    snapshot_recovered_copy: SnapshotDimension,
    snapshot_consistent: SnapshotDimension,
    notes: Vec<String>,
}

fn analyze_copied_set(main_path: &std::path::Path, result: &serde_json::Value) -> CopyAnalysis {
    let mut out = CopyAnalysis {
        file_readable: SnapshotDimension::missed("main database file not copied or unreadable"),
        plaintext_verified: SnapshotDimension::missed("not attempted"),
        structure_interpretable: SnapshotDimension::missed("not attempted"),
        snapshot_recovered_copy: SnapshotDimension::missed("not attempted"),
        snapshot_consistent: SnapshotDimension::missed(
            "a byte copy taken while the source application may be writing cannot prove \
             point-in-time consistency; only the SQLite backup API over a directly readable \
             source can",
        ),
        notes: Vec::new(),
    };
    let Ok(bytes) = std::fs::read(main_path) else {
        return out;
    };
    if bytes.is_empty() {
        out.notes.push("copied main file is empty".to_string());
        return out;
    }
    out.file_readable = SnapshotDimension::reached("copied_bytes_read");
    // The copy exists as bytes; that is exactly what `recovered_copy` claims
    // and nothing more — recovery of a *copy* says nothing about consistency.
    out.snapshot_recovered_copy = SnapshotDimension {
        reached: true,
        evidence: Some("copied_file_set_read_back".to_string()),
        note: Some(
            "the copied byte set was read back; concurrent writes during the copy may be \
             missing or torn, so this is a recovered copy, not a snapshot"
                .to_string(),
        ),
    };

    const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
    if bytes.len() >= 16 && &bytes[..16] == SQLITE_HEADER {
        out.plaintext_verified = SnapshotDimension::reached("sqlite_header");
    } else {
        out.plaintext_verified = SnapshotDimension::missed(
            "first 16 bytes are not the SQLite header: encrypted container or foreign format; which one stays unknown without key material",
        );
        out.notes.push(
            "not a plaintext SQLite header; this plugin does not extract keys and claims nothing about decryptability"
                .to_string(),
        );
        // No schema claims without plaintext.
        return out;
    }

    // Companion presence is concurrency *evidence*, never a consistency
    // proof: a rollback journal can be created and removed between two file
    // copies, and a WAL-mode main file copied alone loses committed frames.
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
        out.notes.push(
            "companion files were copied; the copy is NOT a point-in-time snapshot and \
             snapshot_consistent stays false"
                .to_string(),
        );
    } else {
        out.notes.push(
            "no companion files were observed at copy time, but absence of a journal/WAL is \
             not proof that the copy is consistent: a write in flight during the copy is \
             invisible here"
                .to_string(),
        );
    }

    // Structure on the copy, read-only.
    match std::panic::catch_unwind(|| structure_probe(main_path)) {
        Ok(Ok(probe)) => {
            out.structure_interpretable = SnapshotDimension {
                reached: true,
                evidence: Some("readonly_open_sqlite_master".to_string()),
                note: Some(probe.integrity_note),
            };
        }
        Ok(Err(_)) => {
            out.structure_interpretable =
                SnapshotDimension::missed("sqlite open or sqlite_master query failed on the copy");
        }
        Err(_) => {
            out.structure_interpretable = SnapshotDimension::missed("structure probe panicked");
        }
    }
    out
}

struct StructureProbe {
    /// What `PRAGMA quick_check` said; recorded verbatim so a torn copy shows
    /// up as data instead of being rounded into "queryable".
    integrity_note: String,
}

/// Open the copied database read-only, prove the schema is queryable and run
/// SQLite's own bounded integrity check on the copy. Never runs against a live
/// source path — only against our own copy.
fn structure_probe(path: &std::path::Path) -> std::result::Result<StructureProbe, ()> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = rusqlite::Connection::open_with_flags(path, flags).map_err(|_| ())?;
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| ())?;
    let integrity_note = match connection.query_row("PRAGMA quick_check(1)", [], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(report) if report == "ok" => "quick_check reported no corruption on the copy".to_string(),
        Ok(report) => format!("quick_check reported a problem on the copy: {report}"),
        Err(error) => format!("quick_check could not run on the copy: {error}"),
    };
    Ok(StructureProbe { integrity_note })
}

/// SQLite Online Backup API over a directly readable source — the only
/// path that earns `snapshot_consistent`.
///
/// The source is opened READ-ONLY: this handler must never create, truncate
/// or journal an application's live database just because the device side
/// named a path. An unreadable or absent source therefore fails instead of
/// being materialised as a fresh empty database.
fn run_sqlite_backup(
    source: &std::path::Path,
    into_dir: std::path::PathBuf,
) -> std::result::Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(&into_dir).map_err(|e| e.to_string())?;
    let target = into_dir.join("backup.sqlite3");
    if target.exists() {
        std::fs::remove_file(&target).map_err(|e| e.to_string())?;
    }
    let source_flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let source_connection = rusqlite::Connection::open_with_flags(source, source_flags)
        .map_err(|e| format!("open source read-only: {e}"))?;
    source_connection
        .backup(rusqlite::DatabaseName::Main, &target, None)
        .map_err(|e| format!("backup: {e}"))?;
    Ok(target)
}

fn seal_snapshot_file(
    backend: &AndroidBackend,
    path: &std::path::Path,
) -> std::result::Result<ObjectRef, String> {
    const MAX_SNAPSHOT_FILE_BYTES_LOCAL: u64 = 64 * 1024 * 1024;
    let meta = std::fs::metadata(path).map_err(|e| format!("snapshot stat: {e}"))?;
    if meta.len() > MAX_SNAPSHOT_FILE_BYTES_LOCAL {
        let digest = blake3_file(path)?;
        return Err(format!(
            "snapshot file {} bytes exceeds the 64MiB bounded CAS route (blake3 {digest}); local path retained: {}",
            meta.len(),
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("snapshot read: {e}"))?;
    backend
        .seal_family(&bytes, "application/octet-stream", "mobile-appdb-snapshots", "appdb-snapshot-bytes")
        .map_err(|e| format!("snapshot seal: {e}"))
}

fn blake3_file(path: &std::path::Path) -> std::result::Result<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shenko-mobile-{name}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A batch whose waits would hold the device's single bridge connection
    /// (and this blocking worker) for as long as it likes is rejected before
    /// anything is sent.
    #[test]
    fn unbounded_batch_duration_and_count_are_rejected() {
        let long_wait = vec![MobileActionInput::Wait { ms: 600_000 }];
        assert!(validate_actions(&long_wait).is_err());

        let mut many = Vec::new();
        for _ in 0..=64 {
            many.push(MobileActionInput::Wait { ms: 1 });
        }
        assert!(validate_actions(&many).is_err());

        // Spread over separate short waits the total is still bounded.
        let mut spread = Vec::new();
        for _ in 0..3 {
            spread.push(MobileActionInput::Swipe {
                x1: 0.0,
                y1: 0.0,
                x2: 1.0,
                y2: 1.0,
                duration_ms: 60_000,
            });
        }
        assert!(validate_actions(&spread).is_err());

        assert!(
            validate_actions(&[MobileActionInput::Tap { x: 1.0, y: 2.0 }]).is_ok(),
            "an ordinary short batch stays valid"
        );
    }

    #[test]
    fn text_actions_reject_nul_and_oversized_payloads() {
        assert!(validate_actions(&[MobileActionInput::Text {
            text: "hello\0world".into()
        }])
        .is_err());
        assert!(validate_actions(&[MobileActionInput::Text {
            text: "x".repeat(MAX_TEXT_CHARS + 1)
        }])
        .is_err());
        assert!(validate_actions(&[MobileActionInput::Text {
            text: "普通 text".into()
        }])
        .is_ok());
    }

    /// The device names a path it claims is directly readable. Opening it
    /// read-write would create or journal an application's database on the
    /// device, so a missing source must fail without being materialised.
    #[test]
    fn direct_readable_backup_never_creates_the_source() {
        let dir = scratch_dir("appdb-absent");
        let absent = dir.join("absent.db");
        let result = run_sqlite_backup(&absent, dir.join("backup"));
        assert!(result.is_err(), "a missing source cannot be backed up");
        assert!(!absent.exists(), "opening the source must never create it");
        assert!(
            !dir.join("backup").join("backup.sqlite3").exists(),
            "no empty backup is left behind"
        );
    }

    /// A byte copy while the source application may be writing can never be
    /// reported as a point-in-time consistent snapshot, with or without
    /// companion files.
    #[test]
    fn copied_set_never_claims_consistency() {
        let dir = scratch_dir("appdb-copy");
        let path = dir.join("app.db");
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE message(id INTEGER PRIMARY KEY, body TEXT);\
                     INSERT INTO message(body) VALUES('hello');",
                )
                .unwrap();
        }

        let companion_shapes = [
            serde_json::json!({"wal_present": false, "wal_size": 0, "journal_copied": false}),
            serde_json::json!({"wal_present": true, "wal_size": 4096, "journal_copied": false}),
            serde_json::json!({"wal_present": false, "wal_size": 0, "journal_copied": true}),
        ];
        for companions in companion_shapes {
            let analysis = analyze_copied_set(&path, &companions);
            assert!(
                analysis.snapshot_recovered_copy.reached,
                "a read-back copy is a recovered copy: {companions}"
            );
            assert!(
                !analysis.snapshot_consistent.reached,
                "a raw copy is never a consistent snapshot: {companions}"
            );
            assert!(analysis.file_readable.reached);
            assert!(analysis.plaintext_verified.reached);
            assert!(
                analysis.structure_interpretable.reached,
                "schema probe on the copy"
            );
            let note = analysis
                .structure_interpretable
                .note
                .as_deref()
                .unwrap_or_default();
            assert!(note.contains("quick_check"), "integrity note: {note}");
        }
    }

    /// A foreign or encrypted container stays a separate fact: not plaintext,
    /// so no schema claim follows.
    #[test]
    fn non_sqlite_copy_reports_no_plaintext_and_no_schema() {
        let dir = scratch_dir("appdb-foreign");
        let path = dir.join("app.db");
        std::fs::write(&path, b"not a sqlite database at all").unwrap();
        let analysis = analyze_copied_set(&path, &serde_json::json!({}));
        assert!(analysis.snapshot_recovered_copy.reached);
        assert!(!analysis.snapshot_consistent.reached);
        assert!(!analysis.plaintext_verified.reached);
        assert!(!analysis.structure_interpretable.reached);
    }
}
