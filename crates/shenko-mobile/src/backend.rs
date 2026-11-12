//! Shared live backend for the six Android native kinds.
//!
//! Mirrors the desktop Computer Use backend shape (one Cordis-registered kind
//! per handler over one shared `AndroidBackend`) with the docs/specs/desktop-runtime.md#spec-24-3 display
//! write-lease semantics: one physical device screen is one display domain
//! with at most one live writer epoch; takeover revokes the old epoch so
//! late machine input is rejected; read-only observation never needs a
//! lease. Observation order is a per-session monotonic `revision`, the same
//! comparable notion the desktop provider uses.
//!
//! T1/E1/T2: the operation is armed in the Store before any effect can
//! leave the process; the blocking device work runs on `spawn_blocking`;
//! evidence and terminal state are committed by `record_external_evidence`.
//! Transport ACKs are never application effects; nothing replays.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Result, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_kernel::NativeTaskInvocation;
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    task::TaskRecord,
};

use crate::bridge::{
    BridgeAuthority, BridgeError, BridgeErrorKind, BridgeOp, DeviceBridge,
};
use crate::facts::CapabilityState;
use crate::fingerprint::{
    FingerprintAbsence, FingerprintEvidence, FingerprintRequest, FingerprintScope, KIND_NODE_TREE,
    KIND_NODE_TREE_PREFIX, KIND_PIXELS_ONLY, MAX_FINGERPRINT_DEPTH, MAX_FINGERPRINT_NODES,
    MAX_FINGERPRINT_WINDOWS, parse_response as parse_fingerprint,
};

/// Behavioral contract of the whole plugin set; sealed into every
/// operation's contract ref. Any semantic change = new digest.
const CONTRACT: &[u8] = b"shenko.mobile/v2: on-device bridge over real granted mechanisms (root su argv, Shizuku binder, Dhizuku device-owner, accessibility, normal perms); bridge token authenticates the bridge service incarnation only and never elevates authority; one display domain has one writer epoch and takeover revokes it, and the epoch plus the human-activity feed are re-checked after observation re-verification and before any event is sent; observation revision is per-session monotonic and an action must target pixels its bound observation actually covers; a region request answered with a larger frame is reported unavailable rather than relabelled a crop; every observation binds the device-attested digest of the accessibility node hierarchy the service actually traversed, together with the display/region scope, the traversal bounds, the covered windows and whether that traversal stopped at a bound, so a truncated prefix is labelled a prefix and an absent or failed hierarchy is recorded as an absence instead of a digest; re-verification compares the same evidence stream over the same declared scope, so a changed hierarchy is detected even when the pixels are identical, and hierarchy evidence that disappeared or changed kind or scope is never reported as unchanged; node text is never retained by this node, only the device's digest and structural facts cross the bridge; input receipts are verified per action, so refused or missing receipts make the batch partial instead of complete; appdb snapshots keep located/readable/recovered/consistent/plaintext/structure/semantics as separate facts, never derive consistency from a raw byte copy, and never claim decrypted or semantically complete content; transport ACK is not an application effect; nothing replays";

/// Total capture bound, matching the desktop provider.
const MAX_REGION_PX: i64 = 32 * 1024 * 1024;
/// Largest PNG that can still travel inside the 16 MiB base64 response
/// frame: base64 inflates by 4/3, and the envelope carries other fields.
const MAX_PNG_BYTES: usize = 12 * 1024 * 1024;
const MAX_SHELL_ARGV: usize = 64;
const MAX_SHELL_ARG_BYTES: usize = 4096;
const MAX_SHELL_ENV: usize = 32;
const BINDING_DIGEST_SCHEMA: &str = "mobile-device-binding-v1";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Salted device-binding digest recorded in resource bindings. Raw Android
/// IDs and serials never enter the ledger; only this digest does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceBinding {
    pub digest: String,
    pub method: String,
    pub bound_at_ms: i64,
}

/// Per-domain lease state (one domain = one physical device screen).
#[derive(Debug, Default)]
struct DomainLeaseState {
    epoch: u64,
    holder: Option<LeaseHolder>,
}

/// The display metrics a session is bound to, and where they came from.
struct DisplayMetrics {
    width_px: i32,
    height_px: i32,
    density_dpi: f64,
    rotation_deg: i32,
    /// `probe` when the device reported them; `capture` when the node had to
    /// take one real frame to learn the actual pixel area.
    source: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct LeaseHolder {
    caller: EntityId,
    session: EntityId,
    token: EntityId,
    acquired_at_ms: i64,
}

/// Writer transitions that act on the domain lease. `close` is deliberately
/// not one of them: it is a session lifecycle op.
#[derive(Debug, Clone, Copy)]
enum LeaseTransition {
    Acquire,
    Release,
    Takeover,
}

/// What the external-takeover feed actually said before an input batch.
#[derive(Debug, Clone)]
enum TakeoverCheck {
    /// Human activity since the lease was acquired: the epoch is revoked.
    HumanActivity,
    /// The feed was reached and reported no human activity.
    Clear,
    /// This session has no accessibility feed, so human input is not
    /// detectable at all. Recorded, never treated as "clear".
    FeedDisabled,
    /// The feed exists but could not be read; the screen's human activity is
    /// unknown for this batch.
    FeedUnavailable(String),
}

impl TakeoverCheck {
    fn as_str(&self) -> &'static str {
        match self {
            Self::HumanActivity => "human_activity",
            Self::Clear => "clear",
            Self::FeedDisabled => "feed_disabled",
            Self::FeedUnavailable(_) => "feed_unavailable",
        }
    }
}

/// A live observation session on one device display.
pub struct MobileSession {
    pub session_id: EntityId,
    pub display_id: i32,
    pub opened_by: EntityId,
    pub device_binding: DeviceBinding,
    pub width_px: i32,
    pub height_px: i32,
    pub density_dpi: f64,
    pub rotation_deg: i32,
    /// Whether external (human) takeover detection is real for this session:
    /// true only while the Accessibility feed is enabled.
    pub external_takeover_detection: bool,
    pub next_revision: AtomicU64,
    pub closed: AtomicBool,
}

impl MobileSession {
    fn revision(&self) -> u64 {
        self.next_revision.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// The sealed durable observation; its blake3 digest is the `observation_id`
/// later input must carry. Content-bound: identical bytes ⇒ identical id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MobileObservation {
    pub session_id: EntityId,
    pub domain_id: String,
    pub display_id: i32,
    /// Per-session monotonic observation order (starts at 1), the same
    /// comparable notion as the desktop provider's revision.
    pub revision: u64,
    pub captured_at_ms: i64,
    pub lease_epoch_at_capture: u64,
    pub region: [i32; 4],
    pub viewport: [i32; 2],
    pub density_dpi: f64,
    pub rotation_deg: i32,
    pub scale_factor: f64,
    pub secure_display: bool,
    pub authority_used: String,
    /// Accessibility node-tree hash when available; otherwise the pixel hash
    /// is the fingerprint and this records that fact.
    pub frame_fingerprint: String,
    /// `accessibility_node_tree` (complete traversal within the declared
    /// bounds), `accessibility_node_tree_prefix` (traversal stopped at a
    /// declared bound, so the digest covers that prefix only) or `pixels_only`
    /// (no hierarchy evidence was available at capture).
    pub fingerprint_kind: String,
    /// Canonicalization of `frame_fingerprint` when it is a hierarchy digest;
    /// `None` for the pixel fingerprint.
    #[serde(default)]
    pub fingerprint_algorithm: Option<String>,
    /// What the hierarchy digest actually covered and when it was sampled.
    #[serde(default)]
    pub fingerprint_scope: Option<FingerprintScope>,
    /// Why no hierarchy digest is bound, when none was available at capture.
    /// This is the record that keeps `pixels_only` honest: it says the screen's
    /// semantic hierarchy was not verified, not that it was unchanged.
    #[serde(default)]
    pub fingerprint_absent: Option<FingerprintAbsence>,
    pub pixel_hash: String,
    pub raw_frame_ref: ObjectRef,
}

/// Normalized per-action input receipt.
#[derive(Debug, Clone, Serialize)]
pub struct MobileActionReceipt {
    pub index: usize,
    /// The action kind that was *sent* for this index.
    pub kind: String,
    /// "acked" = the device accepted the event; "not_acked" = the device
    /// refused it; "no_receipt" = the device returned no receipt for this
    /// index at all. Only the first case is delivery; never an app effect.
    pub transport: &'static str,
    pub authority_used: String,
    /// The device's own diagnostic for this index, retained verbatim.
    pub note: Option<String>,
    pub observed_effect: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LeaseNormalized {
    pub lease_token: Option<EntityId>,
    pub epoch: u64,
    pub takeover_revoked: bool,
}

/// One operation's terminal/normalized view and evidence classification.
pub(crate) struct DriveOutcome {
    pub outcome: Option<OperationOutcome>,
    pub effect: EffectStatus,
    pub level: Option<ObservationLevel>,
    pub decode: DecodeStatus,
    pub normalized: serde_json::Value,
}

impl DriveOutcome {
    /// Rejected before any effect could leave the process.
    pub(crate) fn rejected(reason: &'static str, mut normalized: serde_json::Value) -> Self {
        normalized["result"] = serde_json::json!("rejected");
        normalized["reason"] = serde_json::json!(reason);
        Self {
            outcome: Some(OperationOutcome::Failed),
            effect: EffectStatus::None,
            level: Some(ObservationLevel::RemoteCompleted),
            decode: DecodeStatus::Decoded,
            normalized,
        }
    }

    /// The requested mechanism is absent or not granted on this device.
    pub(crate) fn unavailable(reason: &'static str, detail: String) -> Self {
        Self {
            outcome: Some(OperationOutcome::Failed),
            effect: EffectStatus::None,
            level: None,
            decode: DecodeStatus::Decoded,
            normalized: serde_json::json!({
                "result": "unavailable",
                "reason": reason,
                "detail": detail,
            }),
        }
    }

    /// Bridge transport/probe failure; retryable state stays visible.
    pub(crate) fn bridge_failure(error: &BridgeError) -> Self {
        Self {
            outcome: Some(OperationOutcome::Failed),
            effect: EffectStatus::Unknown,
            level: None,
            decode: DecodeStatus::Invalid,
            normalized: serde_json::json!({
                "result": "bridge_error",
                "kind": format!("{:?}", error.kind),
                "message": error.message,
                "retryable": error.retryable,
            }),
        }
    }

    pub(crate) fn done(
        effect: EffectStatus,
        level: ObservationLevel,
        normalized: serde_json::Value,
    ) -> Self {
        Self {
            outcome: Some(OperationOutcome::Succeeded),
            effect,
            level: Some(level),
            decode: DecodeStatus::Decoded,
            normalized,
        }
    }

    /// The device answered, but the payload does not satisfy the operation
    /// contract (missing fields, a frame that is not the shape it claims).
    /// Recorded as undecodable; a read-only op writes nothing, so the effect
    /// stays `None` instead of pretending an unknown side effect.
    pub(crate) fn protocol_violation(reason: &'static str, detail: serde_json::Value) -> Self {
        Self {
            outcome: Some(OperationOutcome::Failed),
            effect: EffectStatus::None,
            level: None,
            decode: DecodeStatus::Invalid,
            normalized: serde_json::json!({
                "result": "protocol_violation",
                "reason": reason,
                "detail": detail,
            }),
        }
    }

    /// The operation gathered real facts but could not reach its goal, and
    /// nothing was written. Callers keep the fact separation in `normalized`.
    pub(crate) fn incomplete(normalized: serde_json::Value) -> Self {
        Self {
            outcome: Some(OperationOutcome::Failed),
            effect: EffectStatus::None,
            level: None,
            decode: DecodeStatus::Decoded,
            normalized,
        }
    }
}

pub(crate) struct DriveCtx {
    pub operation_id: EntityId,
    pub caller: EntityId,
}

#[derive(Default)]
struct BackendState {
    sessions: BTreeMap<EntityId, Arc<MobileSession>>,
    domains: BTreeMap<String, Arc<Mutex<DomainLeaseState>>>,
    capabilities: BTreeMap<String, CapabilityState>,
    probe: Option<serde_json::Value>,
    binding: Option<DeviceBinding>,
}

pub struct AndroidBackend {
    store: Arc<Store>,
    bridge: Arc<dyn DeviceBridge>,
    data_dir: PathBuf,
    state: Mutex<BackendState>,
}

impl AndroidBackend {
    pub fn new(store: Arc<Store>, bridge: Arc<dyn DeviceBridge>, data_dir: PathBuf) -> Self {
        Self {
            store,
            bridge,
            data_dir,
            state: Mutex::new(BackendState::default()),
        }
    }

    pub fn bridge(&self) -> &Arc<dyn DeviceBridge> {
        &self.bridge
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_family(
            &serde_json::to_vec(value)?,
            "application/json",
            "mobile-effects",
            schema,
        )
    }

    fn seal_media(&self, data: &[u8], media: &str, schema: &str) -> Result<ObjectRef> {
        self.seal_family(data, media, "mobile-effects", schema)
    }

    pub(crate) fn seal_family(
        &self,
        data: &[u8],
        media: &str,
        family: &str,
        schema: &str,
    ) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(data, media, family, schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, BackendState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("mobile backend state poisoned"))
    }

    pub(crate) fn plugin_id() -> EntityId {
        "3b9c1a2e-7d4f-4e8a-9b15-6c2e8f0a1d34"
            .parse()
            .expect("fixed mobile plugin UUID")
    }

    pub(crate) fn contract_digest() -> Digest {
        Digest::blake3(CONTRACT)
    }

    /// Probe the device once and cache the facts + binding; later calls
    /// refresh the cached copy when `refresh` is set or nothing is cached.
    pub(crate) fn probe_cached(&self, refresh: bool) -> Result<serde_json::Value, BridgeError> {
        if !refresh {
            if let Some(probe) = &self.state().map_err(|_| BridgeError {
                kind: BridgeErrorKind::Internal,
                message: "mobile backend state poisoned".to_string(),
                retryable: false,
            })?.probe {
                return Ok(probe.clone());
            }
        }
        let result = self.bridge.roundtrip(BridgeOp::Probe)?;
        let mut state = self.state().map_err(|_| BridgeError {
            kind: BridgeErrorKind::Internal,
            message: "mobile backend state poisoned".to_string(),
            retryable: false,
        })?;
        state.probe = Some(result.clone());
        if let Some(digest) = result.pointer("/binding/digest").and_then(|v| v.as_str()) {
            state.binding = Some(DeviceBinding {
                digest: digest.to_string(),
                method: result
                    .pointer("/binding/method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                bound_at_ms: now_ms(),
            });
        }
        if let Some(caps) = result.get("authorities").and_then(|v| v.as_object()) {
            state.capabilities.clear();
            for (kind, value) in caps {
                // An authority whose state vocabulary this node does not know
                // is undetermined, never absent: dropping it would let a
                // caller read "not reported" as "not granted" (docs/specs/mobile-runtime.md#spec-24-5 keeps
                // available/unknown/failed separate, with revalidation terms).
                let parsed = serde_json::from_value::<CapabilityState>(value.clone())
                    .unwrap_or_else(|error| CapabilityState::Unknown {
                        reason: format!(
                            "device reported an unrecognized state for {kind} ({error}); \
                             revalidation requires a bridge that speaks this node's vocabulary"
                        ),
                    });
                state.capabilities.insert(kind.clone(), parsed);
            }
        }
        Ok(result)
    }

    pub(crate) fn cached_capabilities(&self) -> Result<BTreeMap<String, CapabilityState>> {
        Ok(self.state()?.capabilities.clone())
    }

    /// Read back sealed object bytes by their content id (`blake3:<hex>`).
    /// Used to bind later input to an earlier sealed observation.
    pub fn read_object_by_digest_hex(&self, digest: &str) -> Result<Vec<u8>> {
        Ok(self.store.cas.read_object(&Digest(digest.to_string()))?)
    }

    pub(crate) fn device_binding(&self) -> Option<DeviceBinding> {
        self.state().ok()?.binding.clone()
    }

    // ------------------------------------------------------------------
    // Sessions and display domains
    // ------------------------------------------------------------------

    fn domain(&self, display_id: i32) -> Arc<Mutex<DomainLeaseState>> {
        let key = format!("display:{display_id}");
        self.state()
            .expect("mobile backend state poisoned")
            .domains
            .entry(key)
            .or_default()
            .clone()
    }

    pub(crate) fn session(&self, session_id: EntityId) -> Result<Arc<MobileSession>, &'static str> {
        let state = self.state().map_err(|_| "backend_state_poisoned")?;
        match state.sessions.get(&session_id) {
            Some(session) if !session.closed.load(Ordering::Relaxed) => Ok(Arc::clone(session)),
            Some(_) => Err("session_closed"),
            None => Err("session_unknown"),
        }
    }

    /// Open a session: probe the device, verify bridge reachability, and
    /// record the display identity the session is bound to.
    pub(crate) fn open_session(
        self: &Arc<Self>,
        display_id: Option<i32>,
        caller: EntityId,
    ) -> DriveOutcome {
        let probe = match self.probe_cached(true) {
            Ok(probe) => probe,
            Err(error) => return DriveOutcome::bridge_failure(&error),
        };
        let display_id = display_id.unwrap_or(0);
        let display = match self.resolve_display_metrics(display_id, &probe) {
            Ok(display) => display,
            Err(error) => return DriveOutcome::bridge_failure(&error),
        };
        let session = Arc::new(MobileSession {
            session_id: EntityId::new(),
            display_id,
            opened_by: caller,
            device_binding: self.device_binding().unwrap_or(DeviceBinding {
                digest: "unbound".to_string(),
                method: "unbound".to_string(),
                bound_at_ms: now_ms(),
            }),
            width_px: display.width_px,
            height_px: display.height_px,
            density_dpi: display.density_dpi,
            rotation_deg: display.rotation_deg,
            // Only a *typed* available accessibility adapter can observe
            // human input; an unrecognized or failed state means this session
            // has no external-takeover feed, which is recorded as such.
            external_takeover_detection: matches!(
                self.cached_capabilities()
                    .ok()
                    .and_then(|capabilities| capabilities.get("accessibility").cloned()),
                Some(CapabilityState::Available { .. })
            ),
            next_revision: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        let session_id = session.session_id;
        if let Ok(mut state) = self.state() {
            state.sessions.insert(session_id, Arc::clone(&session));
        }
        let normalized = serde_json::json!({
            "result": "opened",
            "session_id": session_id,
            "display_id": session.display_id,
            "viewport": [session.width_px, session.height_px],
            "density_dpi": session.density_dpi,
            "rotation_deg": session.rotation_deg,
            "display_metrics_source": display.source,
            "device_binding": session.device_binding,
            "external_takeover_detection": session.external_takeover_detection,
            "probe": probe,
        });
        DriveOutcome::done(EffectStatus::None, ObservationLevel::PostconditionVerified, normalized)
    }

    /// The display a session is bound to. The probe may carry the metrics
    /// (the bundled bridge does); when it does not, the node takes one real
    /// capture and uses the actual pixels. Fabricating a viewport instead
    /// would invalidate every coordinate later bound to this session.
    fn resolve_display_metrics(
        &self,
        display_id: i32,
        probe: &serde_json::Value,
    ) -> Result<DisplayMetrics, BridgeError> {
        let reported = probe.get("display").unwrap_or(&serde_json::Value::Null);
        let width = reported.get("width_px").and_then(|v| v.as_i64()).unwrap_or(0);
        let height = reported
            .get("height_px")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if width > 0 && height > 0 && width.saturating_mul(height) <= MAX_REGION_PX {
            return Ok(DisplayMetrics {
                width_px: width as i32,
                height_px: height as i32,
                density_dpi: reported
                    .get("density_dpi")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                rotation_deg: reported
                    .get("rotation_deg")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32,
                source: "probe",
            });
        }
        let result = self.bridge.roundtrip(BridgeOp::ScreenCapture {
            display_id,
            region: None,
        })?;
        let frame = decode_capture(&result).map_err(|reason| BridgeError {
            kind: BridgeErrorKind::Internal,
            message: format!("display probe capture: {reason}"),
            retryable: false,
        })?;
        if i64::from(frame.width) * i64::from(frame.height) > MAX_REGION_PX {
            return Err(BridgeError {
                kind: BridgeErrorKind::Unsupported,
                message: format!(
                    "display probe frame {}x{} exceeds the {}px capture bound",
                    frame.width, frame.height, MAX_REGION_PX
                ),
                retryable: false,
            });
        }
        Ok(DisplayMetrics {
            width_px: frame.width as i32,
            height_px: frame.height as i32,
            density_dpi: result
                .get("density_dpi")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            rotation_deg: result
                .get("rotation_deg")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            source: "capture",
        })
    }

    pub(crate) fn close_session(self: &Arc<Self>, session_id: EntityId, caller: EntityId) -> DriveOutcome {
        let Ok(session) = self.session(session_id) else {
            return DriveOutcome::rejected("session_unknown", serde_json::json!({"op":"close"}));
        };
        if session.opened_by != caller {
            return DriveOutcome::rejected(
                "session_owner_mismatch",
                serde_json::json!({"op":"close","session_id":session_id}),
            );
        }
        session.closed.store(true, Ordering::SeqCst);
        // Releasing the domain lease held by this session is idempotent:
        // only the session's own holder entry is cleared, the epoch is not
        // bumped (a close is not a takeover).
        let mut released_lease = false;
        if let Ok(mut domain) = self.domain(session.display_id).lock() {
            if domain.holder.is_some_and(|holder| holder.session == session_id) {
                domain.holder = None;
                released_lease = true;
            }
        }
        if let Ok(mut state) = self.state() {
            let _ = state.sessions.remove(&session_id);
        }
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            serde_json::json!({
                "result": "closed",
                "session_id": session_id,
                "released_lease": released_lease,
                "note": "the session stops being accepted for capture and input; its observations are no longer bound to a live session and no epoch is bumped, so another session's lease on this display is untouched",
            }),
        )
    }

    /// docs/specs/desktop-runtime.md#spec-24-3 lease semantics: one display domain, one live writer epoch.
    /// Takeover revokes the previous epoch; late machine input carrying the
    /// old token or an older epoch is rejected before any event is sent.
    pub(crate) fn lease_op(
        self: &Arc<Self>,
        op: crate::handlers::LeaseOp,
        session_id: EntityId,
        token: Option<EntityId>,
        caller: EntityId,
    ) -> DriveOutcome {
        // `close` is a session lifecycle op, not a writer transition, and it
        // must not take the domain lease lock that it then clears.
        let transition = match op {
            crate::handlers::LeaseOp::Close => return self.close_session(session_id, caller),
            crate::handlers::LeaseOp::Acquire => LeaseTransition::Acquire,
            crate::handlers::LeaseOp::Release => LeaseTransition::Release,
            crate::handlers::LeaseOp::Takeover => LeaseTransition::Takeover,
        };
        let Ok(session) = self.session(session_id) else {
            return DriveOutcome::rejected("session_unknown", serde_json::json!({}));
        };
        let domain = self.domain(session.display_id);
        let mut lease = match domain.lock() {
            Ok(lease) => lease,
            Err(_) => return DriveOutcome::rejected("backend_state_poisoned", serde_json::json!({})),
        };
        let mut takeover_revoked = false;
        match transition {
            LeaseTransition::Acquire => {
                if let Some(holder) = lease.holder {
                    if holder.caller != caller || holder.session != session_id {
                        return DriveOutcome::rejected(
                            "lease_held_by_other_writer",
                            serde_json::json!({
                                "holder_session": holder.session,
                                "epoch": lease.epoch,
                            }),
                        );
                    }
                    // Same caller re-acquiring: new token, same epoch.
                    let token = EntityId::new();
                    lease.holder = Some(LeaseHolder {
                        caller,
                        session: session_id,
                        token,
                        acquired_at_ms: now_ms(),
                    });
                    return DriveOutcome::done(
                        EffectStatus::None,
                        ObservationLevel::PostconditionVerified,
                        serde_json::json!({
                            "result": "acquired",
                            "lease_token": token,
                            "epoch": lease.epoch,
                            "takeover_revoked": false,
                        }),
                    );
                }
                let token = EntityId::new();
                lease.holder = Some(LeaseHolder {
                    caller,
                    session: session_id,
                    token,
                    acquired_at_ms: now_ms(),
                });
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::json!({
                        "result": "acquired",
                        "lease_token": token,
                        "epoch": lease.epoch,
                        "takeover_revoked": false,
                    }),
                )
            }
            LeaseTransition::Release => {
                let Some(holder) = lease.holder else {
                    return DriveOutcome::rejected("lease_not_held", serde_json::json!({}));
                };
                if holder.caller != caller || holder.session != session_id || Some(holder.token) != token {
                    return DriveOutcome::rejected("lease_token_mismatch", serde_json::json!({}));
                }
                lease.holder = None;
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::json!({
                        "result": "released",
                        "lease_token": Option::<EntityId>::None,
                        "epoch": lease.epoch,
                        "takeover_revoked": false,
                    }),
                )
            }
            LeaseTransition::Takeover => {
                // Takeover is the human/bound-controller path: revoke the old
                // epoch so any late machine input is rejected. Held injection
                // state (e.g. a held key) cannot be retracted by us; the
                // rejection happens before new sends, and the outcome below
                // records exactly what was and was not revoked.
                if let Some(holder) = lease.holder {
                    if holder.caller == caller && holder.session == session_id {
                        return DriveOutcome::rejected(
                            "takeover_requires_another_controller",
                            serde_json::json!({"epoch": lease.epoch}),
                        );
                    }
                    takeover_revoked = true;
                }
                lease.epoch += 1;
                lease.holder = None;
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::json!({
                        "result": "taken_over",
                        "lease_token": Option::<EntityId>::None,
                        "epoch": lease.epoch,
                        "takeover_revoked": takeover_revoked,
                        "held_input_retraction": "held accessibility gestures cannot persist; a held shell-injected key cannot be retracted by this plugin and stays unknown",
                    }),
                )
            }
        }
    }

    fn lease_snapshot(&self, display_id: i32) -> (u64, Option<LeaseHolder>) {
        self.domain(display_id)
            .lock()
            .map(|lease| (lease.epoch, lease.holder))
            .unwrap_or((0, None))
    }

    /// Accessibility human-activity check: any human gesture since the lease
    /// was acquired revokes the machine epoch exactly like a takeover.
    ///
    /// The answer is tri-state on purpose. A feed that is disabled and a feed
    /// that failed are different facts, and neither may be reported as
    /// "no human activity": callers record `FeedUnavailable` in the result
    /// instead of silently claiming the screen was clear.
    pub(crate) fn human_takeover_check(
        &self,
        session: &MobileSession,
        lease_acquired_at_ms: i64,
    ) -> TakeoverCheck {
        if !session.external_takeover_detection {
            return TakeoverCheck::FeedDisabled;
        }
        let op = BridgeOp::HumanActivityAfter {
            after_ms: lease_acquired_at_ms,
        };
        let result = match self.bridge.roundtrip(op) {
            Ok(result) => result,
            Err(error) => return TakeoverCheck::FeedUnavailable(error.message),
        };
        match result.get("human_events").and_then(|v| v.as_u64()) {
            Some(0) => TakeoverCheck::Clear,
            Some(_) => TakeoverCheck::HumanActivity,
            None => TakeoverCheck::FeedUnavailable(
                "bridge human-activity response carried no event count".to_string(),
            ),
        }
    }

    pub(crate) fn revoke_for_external_takeover(&self, session: &MobileSession) -> u64 {
        let domain = self.domain(session.display_id);
        if let Ok(mut lease) = domain.lock() {
            lease.epoch += 1;
            lease.holder = None;
            lease.epoch
        } else {
            0
        }
    }

    // ------------------------------------------------------------------
    // Observation
    // ------------------------------------------------------------------

    /// Capture the session display through a granted mechanism and seal the
    /// observation. `bind_lease` records the current domain epoch; the
    /// returned `MobileObservation`'s digest is what input binds to.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        self: &Arc<Self>,
        session: &Arc<MobileSession>,
        region: Option<[i32; 4]>,
        bind_lease: bool,
    ) -> DriveOutcome {
        let op = BridgeOp::ScreenCapture {
            display_id: session.display_id,
            region,
        };
        let result = match self.bridge.roundtrip(op) {
            Ok(result) => result,
            Err(error) => return DriveOutcome::bridge_failure(&error),
        };
        let frame = match decode_capture(&result) {
            Ok(frame) => frame,
            Err(reason) => {
                return DriveOutcome::protocol_violation(
                    reason,
                    serde_json::json!({
                        "png_bytes_present": result.get("png_b64").is_some(),
                    }),
                )
            }
        };
        let png = frame.png;
        let width = frame.width;
        let height = frame.height;
        let rgb = frame.rgb;
        // A region request that comes back as a larger frame was not cropped.
        // Recording it as a crop would mislabel every coordinate bound to the
        // observation, so it is reported as an unsupported capability instead.
        let region = match region {
            None => [0, 0, width as i32, height as i32],
            Some([x, y, requested_width, requested_height]) => {
                if x < 0 || y < 0 {
                    return DriveOutcome::protocol_violation(
                        "capture_region_negative_origin",
                        serde_json::json!({"region": [x, y, requested_width, requested_height]}),
                    );
                }
                if i64::from(requested_width) != i64::from(width)
                    || i64::from(requested_height) != i64::from(height)
                {
                    return DriveOutcome::unavailable(
                        "capture_region_unsupported",
                        format!(
                            "the device answered a {}x{} region request with a {}x{} frame; \
                             this node will not relabel an uncropped frame as that region",
                            requested_width, requested_height, width, height
                        ),
                    );
                }
                [x, y, requested_width, requested_height]
            }
        };
        let pixel_hash = blake3::hash(&rgb).to_hex().to_string();
        let authority_used = result
            .get("authority_used")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let raw_ref = match self.seal_media(&png, "image/png", "mobile-frame") {
            Ok(reference) => reference,
            Err(error) => {
                return DriveOutcome::bridge_failure(&BridgeError {
                    kind: BridgeErrorKind::Io,
                    message: format!("seal capture: {error}"),
                    retryable: false,
                })
            }
        };
        // The semantic fingerprint is sampled separately from the pixels, so
        // the observation records both what it covers and when it was taken.
        // Absent or unusable hierarchy evidence becomes a recorded absence and
        // the pixel hash stays the fingerprint, exactly as declared.
        let frame_captured_at_ms = result.get("captured_at_ms").and_then(|v| v.as_i64());
        let evidence = self.fingerprint(session.display_id, region);
        let (frame_fingerprint, fingerprint_kind, fingerprint_algorithm, fingerprint_scope, fingerprint_absent) =
            match evidence {
                FingerprintEvidence::Available(available) => {
                    let fingerprint_kind = available.kind();
                    let fingerprint_algorithm = Some(available.scope.algorithm.clone());
                    (
                        available.digest,
                        fingerprint_kind,
                        fingerprint_algorithm,
                        Some(available.scope),
                        None,
                    )
                }
                FingerprintEvidence::Absent(absence) => (
                    format!("pixels:{pixel_hash}"),
                    KIND_PIXELS_ONLY,
                    None,
                    None,
                    Some(absence),
                ),
            };
        let lease_epoch = if bind_lease {
            self.lease_snapshot(session.display_id).0
        } else {
            0
        };
        let observation = MobileObservation {
            session_id: session.session_id,
            domain_id: format!("display:{}", session.display_id),
            display_id: session.display_id,
            revision: session.revision(),
            captured_at_ms: frame_captured_at_ms.unwrap_or_else(now_ms),
            lease_epoch_at_capture: lease_epoch,
            region,
            viewport: [width as i32, height as i32],
            density_dpi: result
                .get("density_dpi")
                .and_then(|v| v.as_f64())
                .unwrap_or(session.density_dpi),
            rotation_deg: result
                .get("rotation_deg")
                .and_then(|v| v.as_i64())
                .unwrap_or(i64::from(session.rotation_deg)) as i32,
            scale_factor: 1.0,
            secure_display: result
                .get("secure_display")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            authority_used: authority_used.clone(),
            frame_fingerprint,
            fingerprint_kind: fingerprint_kind.to_string(),
            fingerprint_algorithm,
            fingerprint_scope,
            fingerprint_absent,
            pixel_hash: pixel_hash.clone(),
            raw_frame_ref: raw_ref.clone(),
        };
        // The observation id is the digest of the sealed observation record:
        // identical bytes ⇒ identical id, so later input is content-bound.
        let observation_ref = match self.seal(&observation, "mobile-observation-record") {
            Ok(reference) => reference,
            Err(error) => {
                return DriveOutcome::bridge_failure(&BridgeError {
                    kind: BridgeErrorKind::Io,
                    message: format!("seal observation: {error}"),
                    retryable: false,
                })
            }
        };
        // digest.0 is already the `blake3:<hex>` content id of the sealed
        // record; input binds to it verbatim.
        let observation_id = observation_ref.digest.0.clone();
        let normalized = serde_json::json!({
            "result": "observed",
            "observation_id": observation_id,
            "observation": observation,
            "foreground_component": result.get("foreground_component").cloned().unwrap_or(serde_json::Value::Null),
            // The device reports the mechanism it actually used as
            // `capture_method`; reading a different key silently blanked it.
            "capture_method": result.get("capture_method").cloned().unwrap_or(serde_json::Value::Null),
            "region_is_display_absolute": true,
        });
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            normalized,
        )
    }

    /// One bounded hierarchy fingerprint attempt over exactly the scope the
    /// caller declared.
    ///
    /// The device's answer is validated against the request (display, region,
    /// traversal bounds, digest shape and completeness); an answer this node
    /// cannot place in the declared scope is an absence, never a digest.
    fn fingerprint(&self, display_id: i32, region: [i32; 4]) -> FingerprintEvidence {
        let request = FingerprintRequest {
            display_id,
            region,
            max_nodes: MAX_FINGERPRINT_NODES,
            max_depth: MAX_FINGERPRINT_DEPTH,
            max_windows: MAX_FINGERPRINT_WINDOWS,
        };
        let op = BridgeOp::NodeTreeFingerprint {
            display_id: request.display_id,
            region: request.region,
            max_nodes: request.max_nodes,
            max_depth: request.max_depth,
            max_windows: request.max_windows,
        };
        match self.bridge.roundtrip(op) {
            Ok(result) => parse_fingerprint(&result, &request),
            Err(error) => {
                FingerprintEvidence::Absent(FingerprintAbsence::from_bridge_error(&error))
            }
        }
    }

    /// Re-verify a bound observation before any input is sent.
    ///
    /// Two evidence streams are compared, each inside the scope it was sampled
    /// in:
    /// - pixels: viewport and rotation always, the exact pixel hash when the
    ///   observation's fingerprint *is* its pixels or when the caller demands
    ///   pixel freshness;
    /// - the hierarchy digest bound at capture, which has to be produced again
    ///   by the same canonicalization over the same declared scope with the
    ///   same completeness. A digest that disappeared, changed kind or came
    ///   back over a different scope is *not comparable* — which is not the
    ///   same as "unchanged", so input does not leave.
    pub(crate) fn observation_fresh(
        self: &Arc<Self>,
        session: &Arc<MobileSession>,
        sealed: &MobileObservation,
        require_pixel_fresh: bool,
    ) -> Result<(), &'static str> {
        let op = BridgeOp::ScreenCapture {
            display_id: session.display_id,
            region: Some(sealed.region),
        };
        let result = self
            .bridge
            .roundtrip(op)
            .map_err(|_| "capture_unavailable_for_reverify")?;
        let frame = decode_capture(&result)?;
        let rgb = frame.rgb;
        let (width, height) = (frame.width, frame.height);
        if [width as i32, height as i32] != sealed.viewport {
            return Err("viewport_changed");
        }
        let rotation = result
            .get("rotation_deg")
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::from(session.rotation_deg)) as i32;
        if rotation != sealed.rotation_deg {
            return Err("display_rotated");
        }
        let pixel_hash = blake3::hash(&rgb).to_hex().to_string();
        let evidence = self.fingerprint(session.display_id, sealed.region);
        let hierarchy_bound = sealed.fingerprint_kind == KIND_NODE_TREE
            || sealed.fingerprint_kind == KIND_NODE_TREE_PREFIX;
        if hierarchy_bound {
            let Some(sealed_scope) = sealed.fingerprint_scope.as_ref() else {
                // A hierarchy digest without its scope cannot be re-created or
                // compared; refusing is honest, assuming it still holds is not.
                return Err("fingerprint_scope_missing");
            };
            if sealed.fingerprint_algorithm.as_deref() != Some(sealed_scope.algorithm.as_str()) {
                return Err("fingerprint_scope_missing");
            }
            let available = match evidence {
                FingerprintEvidence::Available(available) => available,
                // The hierarchy was fingerprinted at capture and is gone now:
                // that can never be reported as an unchanged screen.
                FingerprintEvidence::Absent(_) => {
                    return Err("fingerprint_unavailable_for_reverify");
                }
            };
            if available.kind() != sealed.fingerprint_kind.as_str() {
                return Err("fingerprint_scope_changed");
            }
            if !available.scope.comparable_with(sealed_scope) {
                return Err("fingerprint_scope_changed");
            }
            if available.digest != sealed.frame_fingerprint {
                return Err("hierarchy_changed");
            }
        } else {
            // Pixels are this observation's fingerprint: no hierarchy claim was
            // made at capture. If hierarchy evidence is available now, the two
            // samples are different streams, so this observation is not
            // extended into a hierarchy claim it never made.
            if evidence.available().is_some() {
                return Err("fingerprint_kind_changed");
            }
            if format!("pixels:{pixel_hash}") != sealed.frame_fingerprint {
                return Err("pixels_changed");
            }
        }
        if require_pixel_fresh {
            if pixel_hash != sealed.pixel_hash {
                return Err("pixels_changed");
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    /// docs/specs/desktop-runtime.md#spec-24-3: every event is delivered only after the bound observation
    /// re-verifies, the lease epoch is live, and no external takeover
    /// revoked it. Unverified delivery is never claimed complete.
    pub(crate) fn inject(
        self: &Arc<Self>,
        session: &Arc<MobileSession>,
        lease_token: EntityId,
        sealed: &MobileObservation,
        actions: Vec<crate::handlers::MobileActionInput>,
        require_pixel_fresh: bool,
    ) -> DriveOutcome {
        let (epoch, holder) = self.lease_snapshot(session.display_id);
        let Some(holder) = holder else {
            return DriveOutcome::rejected("lease_not_held", serde_json::json!({}));
        };
        if holder.session != session.session_id {
            return DriveOutcome::rejected("lease_session_mismatch", serde_json::json!({}));
        }
        if holder.token != lease_token {
            return DriveOutcome::rejected("lease_token_mismatch", serde_json::json!({ "epoch": epoch }));
        }
        if sealed.session_id != session.session_id {
            return DriveOutcome::rejected(
                "observation_session_mismatch",
                serde_json::json!({ "observation_session": sealed.session_id }),
            );
        }
        if sealed.lease_epoch_at_capture != epoch {
            return DriveOutcome::rejected(
                "stale_lease_epoch",
                serde_json::json!({
                    "capture_epoch": sealed.lease_epoch_at_capture,
                    "current_epoch": epoch,
                }),
            );
        }
        // An action must target pixels its bound observation actually covers:
        // the observation is display-absolute, so a point outside it was never
        // seen at that coordinate.
        if let Some((index, point)) = first_outside_bound_observation(sealed, &actions) {
            return DriveOutcome::rejected(
                "coordinate_outside_bound_observation",
                serde_json::json!({
                    "index": index,
                    "point": point,
                    "observation_region": sealed.region,
                }),
            );
        }
        // No bundled mechanism can hold a key down: `input keyevent` and the
        // accessibility service both deliver complete presses. A batch that
        // asks for a held state is rejected before any event, because an ACK
        // for it would be a delivery that never happened.
        if let Some((index, keycode)) = first_held_key(&actions) {
            return DriveOutcome::rejected(
                "key_down_unsupported",
                serde_json::json!({
                    "index": index,
                    "keycode": keycode,
                    "detail": "the granted mechanisms deliver complete key presses only; send the same keycode with down=false for a press, and expect no held-state delivery",
                }),
            );
        }
        if let Err(reason) = self.observation_fresh(session, sealed, require_pixel_fresh) {
            return DriveOutcome::rejected(
                match reason {
                    "viewport_changed" => "stale_observation_viewport",
                    "display_rotated" => "stale_observation_rotation",
                    "hierarchy_changed" => "stale_observation_hierarchy",
                    "pixels_changed" => "stale_observation_pixels",
                    // Evidence that cannot be compared with what the
                    // observation bound is not evidence that it is unchanged.
                    "fingerprint_unavailable_for_reverify" => {
                        "stale_observation_fingerprint_unavailable"
                    }
                    "fingerprint_kind_changed" => "stale_observation_fingerprint_kind",
                    "fingerprint_scope_changed" | "fingerprint_scope_missing" => {
                        "stale_observation_fingerprint_scope"
                    }
                    _ => "stale_observation_unverifiable",
                },
                serde_json::json!({"detail": reason}),
            );
        }
        // Re-verification runs real device round-trips. During them another
        // controller can take over the domain or a human can touch the
        // screen, so the writer epoch and the human-activity feed are checked
        // again immediately before the batch leaves: sending on the strength
        // of the pre-verification snapshot would deliver late input under a
        // revoked epoch.
        let (epoch_now, holder_now) = self.lease_snapshot(session.display_id);
        let Some(holder_now) = holder_now else {
            return DriveOutcome::rejected(
                "lease_revoked_during_verification",
                serde_json::json!({ "epoch": epoch_now, "revocation": "released_or_taken_over" }),
            );
        };
        if holder_now.token != lease_token || holder_now.session != session.session_id {
            return DriveOutcome::rejected(
                "lease_revoked_during_verification",
                serde_json::json!({
                    "epoch": epoch_now,
                    "revocation": "reissued_to_another_writer",
                }),
            );
        }
        if epoch_now != epoch {
            return DriveOutcome::rejected(
                "stale_lease_epoch",
                serde_json::json!({
                    "capture_epoch": epoch,
                    "current_epoch": epoch_now,
                }),
            );
        }
        let takeover_check = self.human_takeover_check(session, holder_now.acquired_at_ms);
        match &takeover_check {
            TakeoverCheck::HumanActivity => {
                let new_epoch = self.revoke_for_external_takeover(session);
                return DriveOutcome::rejected(
                    "external_takeover_revoked_lease",
                    serde_json::json!({
                        "session_id": session.session_id,
                        "new_epoch": new_epoch,
                    }),
                );
            }
            TakeoverCheck::FeedUnavailable(detail) => {
                // Not a hard gate: the mechanisms remain the authority. The
                // batch proceeds, and the result states that human activity
                // was unknowable for this delivery instead of claiming the
                // screen was clear.
                tracing::warn!(
                    session = %session.session_id,
                    "mobile human-activity feed unavailable before input: {detail}"
                );
            }
            TakeoverCheck::Clear | TakeoverCheck::FeedDisabled => {}
        }
        let bridge_actions: Vec<crate::bridge::BridgeInputAction> = actions
            .iter()
            .map(|action| match action {
                crate::handlers::MobileActionInput::Tap { x, y } => {
                    crate::bridge::BridgeInputAction::Tap { x: *x, y: *y }
                }
                crate::handlers::MobileActionInput::Swipe {
                    x1,
                    y1,
                    x2,
                    y2,
                    duration_ms,
                } => crate::bridge::BridgeInputAction::Swipe {
                    x1: *x1,
                    y1: *y1,
                    x2: *x2,
                    y2: *y2,
                    duration_ms: *duration_ms,
                },
                crate::handlers::MobileActionInput::Key { keycode, down } => {
                    crate::bridge::BridgeInputAction::Key {
                        keycode: *keycode,
                        down: *down,
                    }
                }
                crate::handlers::MobileActionInput::Text { text } => {
                    crate::bridge::BridgeInputAction::Text { text: text.clone() }
                }
                crate::handlers::MobileActionInput::Wait { ms } => {
                    crate::bridge::BridgeInputAction::Wait { ms: *ms }
                }
            })
            .collect();
        let op = BridgeOp::InputBatch {
            actions: bridge_actions,
        };
        let result = match self.bridge.roundtrip(op) {
            Ok(result) => result,
            Err(error) => return DriveOutcome::bridge_failure(&error),
        };
        let Some(raw_receipts) = result.get("receipts").and_then(|v| v.as_array()) else {
            // The batch was sent and the device answered without receipts:
            // no action can be claimed delivered, and the device-side effect
            // is genuinely unknown.
            return DriveOutcome {
                outcome: Some(OperationOutcome::Failed),
                effect: EffectStatus::Unknown,
                level: Some(ObservationLevel::TransportWritten),
                decode: DecodeStatus::Invalid,
                normalized: serde_json::json!({
                    "result": "input_receipts_missing",
                    "session_id": session.session_id,
                    "lease_epoch": epoch,
                    "actions_requested": actions.len(),
                    "detail": "the device answered input_batch without a receipts array; delivery of every action stays unknown and nothing replays",
                }),
            };
        };
        let report = verify_receipts(&actions, raw_receipts, &result);
        let acked = report.receipts.iter().filter(|r| r.transport == "acked").count();
        let all_acked = acked == actions.len();
        let normalized = serde_json::json!({
            "result": if all_acked { "injected" } else { "injected_partial" },
            "session_id": session.session_id,
            "lease_epoch": epoch,
            "actions_requested": actions.len(),
            "actions_acked": acked,
            "actions_refused": report.refused,
            "actions_without_receipt": report.missing,
            "unexpected_receipt_indices": report.unexpected,
            "receipts": report.receipts,
            "external_takeover_check": takeover_check.as_str(),
            "external_takeover_detail": match &takeover_check {
                TakeoverCheck::FeedUnavailable(detail) => serde_json::json!(detail),
                _ => serde_json::Value::Null,
            },
            "transport_note": "device ACK of injected events is transport delivery, never an application effect; refused or unacknowledged delivery stays unknown and nothing replays",
        });
        if all_acked {
            // Transport-level completion; app-level effects of the input are
            // Unknown until observed (docs/specs/desktop-runtime.md#spec-24-4).
            DriveOutcome::done(EffectStatus::Unknown, ObservationLevel::TransportWritten, normalized)
        } else {
            // Some events reached the device and the rest did not: a partial
            // external effect, never a completed batch.
            DriveOutcome {
                outcome: Some(OperationOutcome::Failed),
                effect: if acked == 0 {
                    EffectStatus::Unknown
                } else {
                    EffectStatus::Partial
                },
                level: Some(ObservationLevel::TransportWritten),
                decode: DecodeStatus::Decoded,
                normalized,
            }
        }
    }

    // ------------------------------------------------------------------
    // Shell execution
    // ------------------------------------------------------------------

    pub(crate) fn shell_exec(
        self: &Arc<Self>,
        authority: BridgeAuthority,
        argv: Vec<String>,
        env: Vec<String>,
        timeout_secs: u64,
    ) -> DriveOutcome {
        for arg in &argv {
            if arg.len() > MAX_SHELL_ARG_BYTES {
                return DriveOutcome::rejected("argv_arg_too_large", serde_json::json!({}));
            }
        }
        if argv.len() > MAX_SHELL_ARGV || env.len() > MAX_SHELL_ENV {
            return DriveOutcome::rejected("argv_or_env_too_large", serde_json::json!({}));
        }
        let op = BridgeOp::ShellExec {
            authority,
            argv: argv.clone(),
            env: env.clone(),
            timeout_secs,
        };
        let result = match self.bridge.roundtrip(op) {
            Ok(result) => result,
            Err(error) => return DriveOutcome::bridge_failure(&error),
        };
        let normalized = serde_json::json!({
            "result": if result.get("timed_out").and_then(|v| v.as_bool()) == Some(true) {
                "timed_out"
            } else {
                "exited"
            },
            "authority_used": result.get("authority_used").cloned().unwrap_or(serde_json::Value::Null),
            "exit_code": result.get("exit_code").cloned().unwrap_or(serde_json::Value::Null),
            "stdout_b64": result.get("stdout_b64").cloned().unwrap_or(serde_json::Value::Null),
            "stderr_b64": result.get("stderr_b64").cloned().unwrap_or(serde_json::Value::Null),
            "stdout_truncated": result.get("stdout_truncated").cloned().unwrap_or(serde_json::Value::Null),
            "stderr_truncated": result.get("stderr_truncated").cloned().unwrap_or(serde_json::Value::Null),
            "argv": argv,
            "note": "the operation completed when the process exited or timed out; the device-side effects of the command itself remain Unknown until observed",
        });
        if result.get("timed_out").and_then(|v| v.as_bool()) == Some(true) {
            DriveOutcome {
                outcome: Some(OperationOutcome::Interrupted),
                effect: EffectStatus::Unknown,
                level: Some(ObservationLevel::TransportWritten),
                decode: DecodeStatus::Decoded,
                normalized,
            }
        } else {
            DriveOutcome::done(
                EffectStatus::Unknown,
                ObservationLevel::PostconditionVerified,
                normalized,
            )
        }
    }

    // ------------------------------------------------------------------
    // T1/E1/T2 drive
    // ------------------------------------------------------------------

    pub(crate) async fn drive<I, F>(
        self: &Arc<Self>,
        invocation: NativeTaskInvocation,
        operation_name: &str,
        logical_key: &str,
        frozen: I,
        frozen_schema: &str,
        extra_binding: serde_json::Value,
        acceptance: &str,
        work: F,
    ) -> Result<OperationRecord>
    where
        I: serde::Serialize + Send + 'static,
        F: FnOnce(&Arc<AndroidBackend>, &DriveCtx) -> DriveOutcome + Send + 'static,
    {
        let input_ref = self.seal(&frozen, frozen_schema)?;
        let binding = self.device_binding().unwrap_or(DeviceBinding {
            digest: "unbound".to_string(),
            method: "unbound".to_string(),
            bound_at_ms: now_ms(),
        });
        let bindings = ResourceBindings {
            provider: operation_name.into(),
            account_identity: binding.digest.clone(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "on_device_mobile_bridge".into(),
            extra: serde_json::json!({
                "binding": binding,
                "binding_schema": BINDING_DIGEST_SCHEMA,
                "bridge": extra_binding,
            }),
        };
        let resource = self.seal(&bindings, "mobile-binding")?;
        let acceptance_ref = self.seal(&acceptance, "mobile-acceptance")?;
        let retry = self.seal(
            &"no automatic replay; an armed operation without conclusive evidence remains Unknown; unknown device-side effects are reconciled by observation, never resent",
            "mobile-retry",
        )?;
        let contract_digest = Self::contract_digest();
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: format!("builtin:{operation_name}"),
            },
            symbol: format!("{operation_name}.execute"),
            schema: SchemaRef {
                name: "mobile-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: logical_key.into(),
            operation_name: operation_name.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority,
            acceptance_predicate_ref: acceptance_ref,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let mut attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }

        let ctx = DriveCtx {
            operation_id: invocation.operation_id,
            caller: invocation.context.caller_instance_id,
        };
        let backend = Arc::clone(self);
        let outcome = tokio::task::spawn_blocking(move || work(&backend, &ctx))
            .await
            .map_err(|error| anyhow::anyhow!("mobile worker join: {error}"))?;

        let normalized_bytes = serde_json::to_vec(&outcome.normalized)?;
        let raw = self.seal_media(&normalized_bytes, "application/json", "mobile-observation")?;
        let normalized_ref = Some(self.seal(&outcome.normalized, "mobile-result")?);
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(invocation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: operation_name.into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: outcome.decode,
            observation_level: outcome.level,
            normalized_ref,
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            invocation.operation_id,
            attempt.attempt_id,
            evidence,
            outcome.outcome,
            outcome.effect,
        )
    }
}

/// One decoded capture: the sealed PNG bytes plus the packed RGB pixels the
/// hashes are computed over.
struct CapturedFrame {
    png: Vec<u8>,
    rgb: Vec<u8>,
    width: u32,
    height: u32,
}

/// Decode one `screen_capture` response under the agreed bounds. The bound on
/// the PNG is smaller than the response frame because base64 inflates the
/// payload by 4/3, so a 16 MiB PNG could never have fitted the wire.
fn decode_capture(result: &serde_json::Value) -> Result<CapturedFrame, &'static str> {
    let png_b64 = result
        .get("png_b64")
        .and_then(|value| value.as_str())
        .ok_or("capture_missing_png")?;
    let png = B64
        .decode(png_b64.as_bytes())
        .map_err(|_| "capture_png_b64")?;
    if png.len() > MAX_PNG_BYTES {
        return Err("capture_exceeds_png_bound");
    }
    let (rgb, width, height) = decode_png_rgb(&png)?;
    if i64::from(width) * i64::from(height) > MAX_REGION_PX {
        return Err("capture_exceeds_region_bound");
    }
    Ok(CapturedFrame {
        png,
        rgb,
        width,
        height,
    })
}

/// Decode a PNG into packed RGB8; returns (rgb, width, height).
pub(crate) fn decode_png_rgb(png: &[u8]) -> Result<(Vec<u8>, u32, u32), &'static str> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().map_err(|_| "png_header")?;
    let (color_type, _bit_depth) = reader.output_color_type();
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|_| "png_frame")?;
    let (w, h) = (info.width, info.height);
    let rgb = match color_type {
        png::ColorType::Rgb => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgba => {
            let src = &buf[..info.buffer_size()];
            let mut rgb = Vec::with_capacity(src.len() / 4 * 3);
            for px in src.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            rgb
        }
        _ => return Err("png_unsupported_color"),
    };
    Ok((rgb, w, h))
}

fn action_kind(action: &crate::handlers::MobileActionInput) -> &'static str {
    match action {
        crate::handlers::MobileActionInput::Tap { .. } => "tap",
        crate::handlers::MobileActionInput::Swipe { .. } => "swipe",
        crate::handlers::MobileActionInput::Key { .. } => "key",
        crate::handlers::MobileActionInput::Text { .. } => "text",
        crate::handlers::MobileActionInput::Wait { .. } => "wait",
    }
}

/// First action that asks for a key to be *held down*. The bundled
/// mechanisms (`input keyevent`, accessibility gestures) deliver complete
/// presses only, so such a request has no honest delivery to report.
fn first_held_key(
    actions: &[crate::handlers::MobileActionInput],
) -> Option<(usize, i32)> {
    for (index, action) in actions.iter().enumerate() {
        if let crate::handlers::MobileActionInput::Key { keycode, down: true } = action {
            return Some((index, *keycode));
        }
    }
    None
}

/// First action whose target point lies outside the rectangle the bound
/// observation actually saw. Coordinates are display-absolute, so a point
/// outside that rectangle was never observed at that position.
fn first_outside_bound_observation(
    observation: &MobileObservation,
    actions: &[crate::handlers::MobileActionInput],
) -> Option<(usize, [f64; 2])> {
    let [left, top, width, height] = observation.region;
    let left = f64::from(left);
    let top = f64::from(top);
    let right = left + f64::from(width);
    let bottom = top + f64::from(height);
    let inside = |x: f64, y: f64| {
        x.is_finite() && y.is_finite() && x >= left && x < right && y >= top && y < bottom
    };
    for (index, action) in actions.iter().enumerate() {
        match action {
            crate::handlers::MobileActionInput::Tap { x, y } => {
                if !inside(*x, *y) {
                    return Some((index, [*x, *y]));
                }
            }
            crate::handlers::MobileActionInput::Swipe { x1, y1, x2, y2, .. } => {
                if !inside(*x1, *y1) {
                    return Some((index, [*x1, *y1]));
                }
                if !inside(*x2, *y2) {
                    return Some((index, [*x2, *y2]));
                }
            }
            crate::handlers::MobileActionInput::Key { .. }
            | crate::handlers::MobileActionInput::Text { .. }
            | crate::handlers::MobileActionInput::Wait { .. } => {}
        }
    }
    None
}

struct BatchReport {
    receipts: Vec<MobileActionReceipt>,
    refused: Vec<usize>,
    missing: Vec<usize>,
    /// Receipt indices the device reported that this batch never sent, could
    /// not parse, or repeated.
    unexpected: Vec<serde_json::Value>,
}

/// Join the device's per-action receipts back onto the batch that was sent.
///
/// A receipt exists only for the indices the device answered for, and an
/// `acked: false` receipt is a refusal, not delivery. Counting receipts
/// instead of sent actions would let a partially answered batch look
/// complete, so every requested index is accounted for exactly once.
fn verify_receipts(
    actions: &[crate::handlers::MobileActionInput],
    raw: &[serde_json::Value],
    response: &serde_json::Value,
) -> BatchReport {
    let batch_authority = response
        .get("authority_used")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let mut receipts = Vec::with_capacity(actions.len());
    let mut refused = Vec::new();
    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    let mut seen = vec![false; actions.len()];
    for item in raw {
        let Some(index) = item
            .get("index")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize)
        else {
            unexpected.push(serde_json::Value::Null);
            continue;
        };
        if index >= actions.len() || seen[index] {
            unexpected.push(serde_json::json!(index));
            continue;
        }
        seen[index] = true;
        let acked = item.get("acked").and_then(|value| value.as_bool()) == Some(true);
        receipts.push(MobileActionReceipt {
            index,
            kind: action_kind(&actions[index]).to_string(),
            transport: if acked { "acked" } else { "not_acked" },
            authority_used: item
                .get("authority_used")
                .and_then(|value| value.as_str())
                .unwrap_or(batch_authority.as_str())
                .to_string(),
            note: item
                .get("note")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            observed_effect: None,
        });
        if !acked {
            refused.push(index);
        }
    }
    for (index, answered) in seen.iter().enumerate() {
        if *answered {
            continue;
        }
        missing.push(index);
        receipts.push(MobileActionReceipt {
            index,
            kind: action_kind(&actions[index]).to_string(),
            transport: "no_receipt",
            authority_used: batch_authority.clone(),
            note: None,
            observed_effect: None,
        });
    }
    receipts.sort_by_key(|receipt| receipt.index);
    BatchReport {
        receipts,
        refused,
        missing,
        unexpected,
    }
}

pub(crate) async fn reconcile_mobile(
    backend: &Arc<AndroidBackend>,
    task: &TaskRecord,
    operation_id: EntityId,
    operation_name: &str,
) -> Result<OperationRecord> {
    let operation = backend.store.external_operation(operation_id)?;
    ensure!(
        operation.task_id == task.task_id && operation.operation_name == operation_name,
        "original_operation_contract_mismatch"
    );
    if operation.state != OperationState::InFlight {
        return Ok(operation);
    }
    let attempt = backend.store.external_operation_attempt(operation_id)?;
    if attempt.dispatch_stage == AttemptDispatchStage::Ended {
        return Ok(operation);
    }
    let raw = backend.seal(
        &serde_json::json!({
            "operation_id": operation_id,
            "attempt_id": attempt.attempt_id,
            "observation": "controller recovered without a conclusive result; the on-device effect (if any) is Unknown and must be observed, never resent",
            "process_stop_proven": false,
        }),
        "mobile-recovery",
    )?;
    let evidence = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(operation_id),
        attempt_id: Some(attempt.attempt_id),
        source_id: format!("{operation_name}.recovery"),
        source_instance_id: backend.store.capabilities.incarnation.to_string(),
        source_event_key: Some(format!("controller-recovery:{}", attempt.attempt_id)),
        producer_epoch: None,
        producer_schema_version: Some("1".into()),
        raw_ref: raw,
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: DecodeStatus::Decoded,
        observation_level: None,
        normalized_ref: None,
        observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    };
    backend.store.record_external_evidence(
        operation_id,
        attempt.attempt_id,
        evidence,
        None,
        EffectStatus::Unknown,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::{LeaseOp, MobileActionInput};
    use std::sync::atomic::AtomicUsize;

    type Responder =
        Box<dyn Fn(&BridgeOp) -> Result<serde_json::Value, BridgeError> + Send + Sync>;

    struct FakeBridge {
        responder: Responder,
        seen: Mutex<Vec<BridgeOp>>,
    }

    impl FakeBridge {
        fn new(responder: impl Fn(&BridgeOp) -> Result<serde_json::Value, BridgeError> + Send + Sync + 'static) -> Self {
            Self {
                responder: Box::new(responder),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<BridgeOp> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl DeviceBridge for FakeBridge {
        fn roundtrip(&self, op: BridgeOp) -> Result<serde_json::Value, BridgeError> {
            self.seen.lock().unwrap().push(op.clone());
            (self.responder)(&op)
        }
    }

    struct Fixture {
        backend: Arc<AndroidBackend>,
        bridge: Arc<FakeBridge>,
    }

    fn fixture(
        responder: impl Fn(&BridgeOp) -> Result<serde_json::Value, BridgeError> + Send + Sync + 'static,
    ) -> Fixture {
        let dir = std::env::temp_dir().join(format!("shenko-mobile-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(
            Store::open(dir.join("store.db"), dir.join("cas"))
                .expect("open test store"),
        );
        let bridge = Arc::new(FakeBridge::new(responder));
        let backend = Arc::new(AndroidBackend::new(
            store,
            Arc::clone(&bridge) as Arc<dyn DeviceBridge>,
            dir,
        ));
        Fixture { backend, bridge }
    }

    fn png_b64(width: u32, height: u32) -> String {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&vec![9u8; (width * height * 3) as usize])
                .unwrap();
        }
        B64.encode(&bytes)
    }

    fn probe_value() -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "binding": {"digest": "test-binding", "method": "app_install_id"},
            "authorities": {
                "normal_perm": {"state": "available", "detail": "app uid", "verified_at_ms": 1},
                "accessibility": {"state": "unavailable", "reason": "not enabled", "revalidate": "enable"},
            },
            "display": {"display_id": 0, "width_px": 64, "height_px": 48, "density_dpi": 320.0, "rotation_deg": 0},
        })
    }

    fn capture_value(width: u32, height: u32) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "png_b64": png_b64(width, height),
            "width_px": width,
            "height_px": height,
            "density_dpi": 320.0,
            "rotation_deg": 0,
            "secure_display": false,
            "capture_method": "screencap_argv",
            "captured_at_ms": 1,
            "authority_used": "shizuku",
            "display": {"display_id": 0},
        })
    }

    fn open_session_ready(fixture: &Fixture, caller: EntityId) -> (EntityId, Arc<MobileSession>) {
        let opened = fixture.backend.open_session(None, caller);
        assert_eq!(opened.outcome, Some(OperationOutcome::Succeeded), "{:?}", opened.normalized);
        let session_id: EntityId = opened.normalized["session_id"]
            .as_str()
            .expect("session id")
            .parse()
            .expect("session uuid");
        let session = fixture.backend.session(session_id).expect("live session");
        (session_id, session)
    }

    /// A digest of the shape the device must produce.
    fn hierarchy_digest(seed: char) -> String {
        format!("sha256:{}", seed.to_string().repeat(64))
    }

    /// A device answer for a fingerprint request, echoing the requested scope
    /// because that is what the node validates against.
    fn fingerprint_value(
        digest: &str,
        node_filter: &str,
        truncation_reason: &str,
        node_count: u32,
    ) -> serde_json::Value {
        let mut value = serde_json::json!({
            "ok": true,
            "fingerprint": digest,
            "algorithm": crate::fingerprint::FINGERPRINT_ALGORITHM,
            "display_id": 0,
            "region": [0, 0, 64, 48],
            "node_filter": node_filter,
            "max_nodes": crate::fingerprint::MAX_FINGERPRINT_NODES,
            "max_depth": crate::fingerprint::MAX_FINGERPRINT_DEPTH,
            "max_windows": crate::fingerprint::MAX_FINGERPRINT_WINDOWS,
            "windows_considered": 1,
            "window_packages": ["com.example.app"],
            "visited_nodes": node_count,
            "node_count": node_count,
            "truncated": !truncation_reason.is_empty(),
            "sampled_at_ms": 1_700_000_000_000i64,
            "elapsed_ms": 3,
            "notes": [],
        });
        if !truncation_reason.is_empty() {
            value["truncation_reason"] = serde_json::json!(truncation_reason);
        }
        value
    }

    /// The honest off-device / service-not-enabled answer: no digest exists.
    fn no_hierarchy() -> Result<serde_json::Value, BridgeError> {
        Err(BridgeError::typed_unavailable(
            "accessibility service not enabled; the node hierarchy is not observable",
        ))
    }

    fn sealed_observation(observed: &DriveOutcome) -> MobileObservation {
        serde_json::from_value(observed.normalized["observation"].clone()).expect("observation")
    }

    fn lease_for(fixture: &Fixture, session_id: EntityId, caller: EntityId) -> EntityId {
        let acquired = fixture.backend.lease_op(LeaseOp::Acquire, session_id, None, caller);
        acquired.normalized["lease_token"]
            .as_str()
            .expect("lease token")
            .parse()
            .expect("lease uuid")
    }

    /// A takeover that lands *during* observation re-verification must not
    /// deliver the batch: re-verification runs real device round-trips, so
    /// the pre-verification epoch snapshot is already stale when the batch
    /// would leave.
    #[test]
    fn takeover_during_verification_rejects_batch_without_sending() {
        let takeover_slot: Arc<Mutex<Option<std::sync::Weak<AndroidBackend>>>> =
            Arc::new(Mutex::new(None));
        let captures = Arc::new(AtomicUsize::new(0));
        let slot = Arc::clone(&takeover_slot);
        let captures_for_responder = Arc::clone(&captures);
        let other_caller = EntityId::new();
        let session_for_takeover: Arc<Mutex<Option<EntityId>>> = Arc::new(Mutex::new(None));
        let session_for_responder = Arc::clone(&session_for_takeover);
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => {
                if captures_for_responder.fetch_add(1, Ordering::SeqCst) == 1 {
                    // Second capture is the pre-send re-verification.
                    let backend = slot
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(std::sync::Weak::upgrade)
                        .expect("backend alive");
                    let session_id = session_for_responder
                        .lock()
                        .unwrap()
                        .expect("session recorded");
                    let outcome = backend.lease_op(LeaseOp::Takeover, session_id, None, other_caller);
                    assert_eq!(outcome.outcome, Some(OperationOutcome::Succeeded));
                }
                Ok(capture_value(64, 48))
            }
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        *takeover_slot.lock().unwrap() = Some(Arc::downgrade(&fixture.backend));
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        *session_for_takeover.lock().unwrap() = Some(session_id);

        let acquired = fixture.backend.lease_op(LeaseOp::Acquire, session_id, None, caller);
        let token: EntityId = acquired.normalized["lease_token"]
            .as_str()
            .expect("lease token")
            .parse()
            .expect("lease uuid");
        let observed = fixture.backend.capture(&session, None, true);
        let sealed: MobileObservation =
            serde_json::from_value(observed.normalized["observation"].clone()).expect("observation");

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(outcome.normalized["result"], "rejected");
        assert_eq!(
            outcome.normalized["reason"],
            "lease_revoked_during_verification"
        );
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "no event may be sent after the epoch was revoked"
        );
    }

    /// A device that answers a region request with a larger frame must not be
    /// relabelled as a crop: every coordinate bound to that observation would
    /// point at the wrong pixels.
    #[test]
    fn region_request_answered_with_full_frame_is_unavailable() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (_session_id, session) = open_session_ready(&fixture, caller);
        let outcome = fixture
            .backend
            .capture(&session, Some([0, 0, 16, 16]), true);
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(outcome.normalized["result"], "unavailable");
        assert_eq!(outcome.normalized["reason"], "capture_region_unsupported");
        assert!(outcome.normalized.get("observation_id").is_none());
    }

    /// An action outside the rectangle the observation actually saw was never
    /// observed there, so it is rejected instead of being sent.
    #[test]
    fn tap_outside_bound_observation_is_rejected_before_send() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let acquired = fixture.backend.lease_op(LeaseOp::Acquire, session_id, None, caller);
        let token: EntityId = acquired.normalized["lease_token"]
            .as_str()
            .expect("lease token")
            .parse()
            .expect("lease uuid");
        let observed = fixture.backend.capture(&session, None, true);
        let sealed: MobileObservation =
            serde_json::from_value(observed.normalized["observation"].clone()).expect("observation");
        assert_eq!(sealed.region, [0, 0, 64, 48]);

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }, MobileActionInput::Tap {
                x: 4000.0,
                y: 10.0,
            }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            outcome.normalized["reason"],
            "coordinate_outside_bound_observation"
        );
        assert_eq!(outcome.normalized["index"], 1);
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "nothing may be sent when a coordinate was never observed"
        );
    }

    /// `input keyevent` and accessibility gestures deliver complete presses.
    /// A batch asking for a held key has nothing honest to report, so it is
    /// rejected before any event instead of being acknowledged.
    #[test]
    fn held_key_request_is_rejected_before_send() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let acquired = fixture.backend.lease_op(LeaseOp::Acquire, session_id, None, caller);
        let token: EntityId = acquired.normalized["lease_token"]
            .as_str()
            .expect("lease token")
            .parse()
            .expect("lease uuid");
        let observed = fixture.backend.capture(&session, None, true);
        let sealed: MobileObservation =
            serde_json::from_value(observed.normalized["observation"].clone()).expect("observation");

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Key {
                keycode: 59,
                down: true,
            }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(outcome.normalized["reason"], "key_down_unsupported");
        assert_eq!(outcome.normalized["keycode"], 59);
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "an undeliverable held key must not be sent"
        );
    }

    /// Refused and unanswered actions are counted per sent index; a batch
    /// that the device only partially acknowledged is a partial effect, not a
    /// successful injection.
    #[test]
    fn refused_and_missing_receipts_make_the_batch_partial() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::InputBatch { .. } => Ok(serde_json::json!({
                "ok": true,
                "authority_used": "shizuku",
                "receipts": [
                    {"index": 0, "acked": true, "note": "input tap", "authority_used": "shizuku"},
                    {"index": 1, "acked": false, "note": "no granted shell authority", "authority_used": "none"},
                ],
            })),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let acquired = fixture.backend.lease_op(LeaseOp::Acquire, session_id, None, caller);
        let token: EntityId = acquired.normalized["lease_token"]
            .as_str()
            .expect("lease token")
            .parse()
            .expect("lease uuid");
        let observed = fixture.backend.capture(&session, None, true);
        let sealed: MobileObservation =
            serde_json::from_value(observed.normalized["observation"].clone()).expect("observation");

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![
                MobileActionInput::Tap { x: 1.0, y: 1.0 },
                MobileActionInput::Text {
                    text: "hello".into(),
                },
                MobileActionInput::Wait { ms: 5 },
            ],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(outcome.effect, EffectStatus::Partial);
        assert_eq!(outcome.normalized["result"], "injected_partial");
        assert_eq!(outcome.normalized["actions_requested"], 3);
        assert_eq!(outcome.normalized["actions_acked"], 1);
        assert_eq!(outcome.normalized["actions_refused"], serde_json::json!([1]));
        assert_eq!(
            outcome.normalized["actions_without_receipt"],
            serde_json::json!([2])
        );
        let receipts = outcome.normalized["receipts"].as_array().expect("receipts");
        assert_eq!(receipts.len(), 3);
        assert_eq!(receipts[1]["transport"], "not_acked");
        assert_eq!(receipts[1]["note"], "no granted shell authority");
        assert_eq!(receipts[1]["authority_used"], "none");
        assert_eq!(receipts[2]["transport"], "no_receipt");
    }

    /// An authority state this node does not understand is undetermined, not
    /// absent; a typed available accessibility adapter enables the human
    /// activity feed for the session.
    #[test]
    fn unrecognized_authority_state_is_unknown_and_not_absent() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(serde_json::json!({
                "ok": true,
                "binding": {"digest": "test-binding", "method": "app_install_id"},
                "authorities": {
                    "accessibility": {"state": "available", "detail": "enabled", "verified_at_ms": 1},
                    "notification_listener": {"state": "listening_v2", "detail": "future vocabulary"},
                },
                "display": {"display_id": 0, "width_px": 32, "height_px": 24, "density_dpi": 160.0, "rotation_deg": 0},
            })),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (_session_id, session) = open_session_ready(&fixture, caller);
        let capabilities = fixture
            .backend
            .cached_capabilities()
            .expect("capability cache");
        assert!(matches!(
            capabilities.get("accessibility"),
            Some(CapabilityState::Available { .. })
        ));
        assert!(matches!(
            capabilities.get("notification_listener"),
            Some(CapabilityState::Unknown { .. })
        ));
        assert!(session.external_takeover_detection);
    }

    /// Closing a session ends it, releases only its own writer lease, and
    /// does not revoke another writer's epoch on the same display.
    #[test]
    fn session_close_releases_only_its_own_lease() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(32, 24)),
            // No accessibility service in these fixtures: the capture binds
            // pixel evidence only, exactly as the absent-hierarchy path does
            // on a device without the service enabled.
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (first_id, _first) = open_session_ready(&fixture, caller);
        let second_open = fixture.backend.open_session(Some(0), caller);
        let second_id: EntityId = second_open.normalized["session_id"]
            .as_str()
            .expect("second session")
            .parse()
            .expect("second uuid");

        let acquired = fixture.backend.lease_op(LeaseOp::Acquire, first_id, None, caller);
        assert_eq!(acquired.outcome, Some(OperationOutcome::Succeeded));
        let epoch_before = fixture.backend.lease_snapshot(0).0;

        let closed = fixture.backend.lease_op(LeaseOp::Close, first_id, None, caller);
        assert_eq!(closed.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(closed.normalized["released_lease"], true);
        assert!(fixture.backend.session(first_id).is_err());
        assert!(fixture.backend.lease_snapshot(0).1.is_none());
        assert_eq!(fixture.backend.lease_snapshot(0).0, epoch_before);

        // Another session on the same display can now take the writer slot.
        let reacquired = fixture.backend.lease_op(LeaseOp::Acquire, second_id, None, caller);
        assert_eq!(reacquired.outcome, Some(OperationOutcome::Succeeded));

        // A different caller may not close this session.
        let foreign = fixture
            .backend
            .lease_op(LeaseOp::Close, second_id, None, EntityId::new());
        assert_eq!(foreign.normalized["reason"], "session_owner_mismatch");
        // Closing a session that no longer exists is rejected, not silent.
        let again = fixture.backend.lease_op(LeaseOp::Close, first_id, None, caller);
        assert_eq!(again.normalized["reason"], "session_unknown");
    }

    // ------------------------------------------------------------------
    // Hierarchy fingerprint producer → observation → re-verification
    // ------------------------------------------------------------------

    /// The capture asks the device for the hierarchy over exactly the area the
    /// observation covers, under the declared traversal bounds, and binds the
    /// returned digest with its scope.
    #[test]
    fn capture_binds_the_device_digest_with_its_declared_scope() {
        let digest = hierarchy_digest('a');
        let digest_for_responder = digest.clone();
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => {
                Ok(fingerprint_value(&digest_for_responder, "bounds_intersect_region", "", 12))
            }
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let observed = fixture.backend.capture(&session, None, true);
        assert_eq!(
            observed.outcome,
            Some(OperationOutcome::Succeeded),
            "{:?}",
            observed.normalized
        );
        let sealed = sealed_observation(&observed);
        assert_eq!(sealed.fingerprint_kind, KIND_NODE_TREE);
        assert_eq!(sealed.frame_fingerprint, digest);
        assert_eq!(
            sealed.fingerprint_algorithm.as_deref(),
            Some(crate::fingerprint::FINGERPRINT_ALGORITHM)
        );
        assert!(sealed.fingerprint_absent.is_none());
        let scope = sealed.fingerprint_scope.expect("scope bound beside the digest");
        assert_eq!(scope.region, sealed.region);
        assert_eq!(scope.display_id, session.display_id);
        assert_eq!(scope.node_filter, "bounds_intersect_region");
        assert_eq!(scope.node_count, 12);
        assert!(!scope.truncated);

        // The request itself described that same scope, not the whole display
        // under some other bound.
        assert!(
            fixture.bridge.seen().iter().any(|op| matches!(
                op,
                BridgeOp::NodeTreeFingerprint {
                    display_id,
                    region,
                    max_nodes,
                    max_depth,
                    max_windows,
                } if *display_id == session.display_id
                    && *region == sealed.region
                    && *max_nodes == crate::fingerprint::MAX_FINGERPRINT_NODES
                    && *max_depth == crate::fingerprint::MAX_FINGERPRINT_DEPTH
                    && *max_windows == crate::fingerprint::MAX_FINGERPRINT_WINDOWS
            )),
            "fingerprint request must carry the observed scope: {:?}",
            fixture.bridge.seen()
        );
        assert!(fixture.backend.session(session_id).is_ok());
    }

    /// A changed semantic hierarchy with byte-identical pixels is a changed
    /// screen: the bound digest no longer matches, so no input leaves.
    #[test]
    fn hierarchy_change_with_unchanged_pixels_never_delivers_input() {
        let samples = Arc::new(AtomicUsize::new(0));
        let samples_for_responder = Arc::clone(&samples);
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            // The pixels are identical on every capture.
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => {
                let digest = if samples_for_responder.fetch_add(1, Ordering::SeqCst) == 0 {
                    hierarchy_digest('a')
                } else {
                    hierarchy_digest('b')
                };
                Ok(fingerprint_value(&digest, "bounds_intersect_region", "", 12))
            }
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let token = lease_for(&fixture, session_id, caller);
        let observed = fixture.backend.capture(&session, None, false);
        let sealed = sealed_observation(&observed);

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(outcome.normalized["result"], "rejected");
        assert_eq!(outcome.normalized["reason"], "stale_observation_hierarchy");
        assert_eq!(outcome.normalized["detail"], "hierarchy_changed");
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "a changed hierarchy must not deliver the batch"
        );
    }

    /// The same hierarchy and the same pixels over the same scope is the only
    /// combination that lets the batch leave, and delivery is still transport
    /// ACK, not an application effect.
    #[test]
    fn stable_hierarchy_and_pixels_allow_delivery() {
        let digest = hierarchy_digest('c');
        let digest_for_responder = digest.clone();
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => {
                Ok(fingerprint_value(&digest_for_responder, "bounds_intersect_region", "", 12))
            }
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            BridgeOp::InputBatch { actions } => Ok(serde_json::json!({
                "ok": true,
                "authority_used": "shizuku",
                "receipts": (0..actions.len())
                    .map(|index| serde_json::json!({
                        "index": index,
                        "acked": true,
                        "note": "input tap",
                        "authority_used": "shizuku",
                    }))
                    .collect::<Vec<_>>(),
            })),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let token = lease_for(&fixture, session_id, caller);
        let observed = fixture.backend.capture(&session, None, false);
        let sealed = sealed_observation(&observed);
        assert_eq!(sealed.frame_fingerprint, digest);

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(
            outcome.outcome,
            Some(OperationOutcome::Succeeded),
            "{:?}",
            outcome.normalized
        );
        assert_eq!(outcome.normalized["result"], "injected");
        assert_eq!(outcome.normalized["actions_acked"], 1);
        assert_eq!(outcome.effect, EffectStatus::Unknown);
    }

    /// Without a hierarchy the observation says so: the pixel hash is the
    /// fingerprint, the absent reason is recorded, and no hierarchy claim is
    /// made. Unchanged pixels still allow delivery under that pixel contract.
    #[test]
    fn absent_hierarchy_at_capture_is_recorded_as_pixels_only() {
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => no_hierarchy(),
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            BridgeOp::InputBatch { actions } => Ok(serde_json::json!({
                "ok": true,
                "authority_used": "shizuku",
                "receipts": (0..actions.len())
                    .map(|index| serde_json::json!({"index": index, "acked": true}))
                    .collect::<Vec<_>>(),
            })),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let token = lease_for(&fixture, session_id, caller);
        let observed = fixture.backend.capture(&session, None, false);
        let sealed = sealed_observation(&observed);
        assert_eq!(sealed.fingerprint_kind, KIND_PIXELS_ONLY);
        assert!(sealed.frame_fingerprint.starts_with("pixels:"));
        assert!(sealed.fingerprint_scope.is_none());
        assert!(sealed.fingerprint_algorithm.is_none());
        let absence = sealed.fingerprint_absent.as_ref().expect("absence is a recorded fact");
        assert_eq!(absence.reason, "hierarchy_unavailable");
        assert!(absence.detail.contains("accessibility"));
        assert!(
            observed.normalized["observation"]["fingerprint_absent"]["reason"]
                == serde_json::json!("hierarchy_unavailable"),
            "the absence travels with the observation the caller reads"
        );

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(outcome.normalized["result"], "injected");
    }

    /// Hierarchy evidence that appears after a pixels-only capture cannot
    /// retroactively become "the same hierarchy": the observation never made
    /// that claim, so the evidence streams are not comparable.
    #[test]
    fn hierarchy_appearing_after_a_pixels_only_capture_is_not_unchanged() {
        let fingerprint_calls = Arc::new(AtomicUsize::new(0));
        let calls_for_responder = Arc::clone(&fingerprint_calls);
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => {
                if calls_for_responder.fetch_add(1, Ordering::SeqCst) == 0 {
                    no_hierarchy()
                } else {
                    Ok(fingerprint_value(
                        &hierarchy_digest('d'),
                        "bounds_intersect_region",
                        "",
                        12,
                    ))
                }
            }
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let token = lease_for(&fixture, session_id, caller);
        let observed = fixture.backend.capture(&session, None, false);
        let sealed = sealed_observation(&observed);
        assert_eq!(sealed.fingerprint_kind, KIND_PIXELS_ONLY);

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            outcome.normalized["reason"],
            "stale_observation_fingerprint_kind"
        );
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "a pixels-only observation cannot be extended into a hierarchy claim"
        );
    }

    /// Hierarchy evidence that disappears between capture and delivery is not
    /// an unchanged screen, even when the pixels are identical.
    #[test]
    fn hierarchy_unavailable_on_reverify_is_not_unchanged() {
        let fingerprint_calls = Arc::new(AtomicUsize::new(0));
        let calls_for_responder = Arc::clone(&fingerprint_calls);
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => {
                if calls_for_responder.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(fingerprint_value(
                        &hierarchy_digest('e'),
                        "bounds_intersect_region",
                        "",
                        12,
                    ))
                } else {
                    no_hierarchy()
                }
            }
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let token = lease_for(&fixture, session_id, caller);
        let observed = fixture.backend.capture(&session, None, false);
        let sealed = sealed_observation(&observed);
        assert_eq!(sealed.fingerprint_kind, KIND_NODE_TREE);

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            outcome.normalized["reason"],
            "stale_observation_fingerprint_unavailable"
        );
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "missing hierarchy evidence must not become an unchanged claim"
        );
    }

    /// A traversal that stopped at a declared bound is bound as a prefix, and
    /// a re-observation that covers a different amount is a different scope.
    #[test]
    fn truncated_prefix_is_declared_and_rescoped_evidence_is_not_unchanged() {
        let fingerprint_calls = Arc::new(AtomicUsize::new(0));
        let calls_for_responder = Arc::clone(&fingerprint_calls);
        let fixture = fixture(move |op| match op {
            BridgeOp::Probe => Ok(probe_value()),
            BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
            BridgeOp::NodeTreeFingerprint { .. } => {
                let digest = hierarchy_digest('f');
                if calls_for_responder.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(fingerprint_value(
                        &digest,
                        "bounds_intersect_region",
                        "node_budget",
                        crate::fingerprint::MAX_FINGERPRINT_NODES,
                    ))
                } else {
                    // Same digest shape, but the service could only apply a
                    // different inclusion rule: a prefix of a different node
                    // set says nothing about the captured prefix.
                    Ok(fingerprint_value(
                        &digest,
                        "visited_nodes_only",
                        "node_budget",
                        crate::fingerprint::MAX_FINGERPRINT_NODES,
                    ))
                }
            }
            BridgeOp::HumanActivityAfter { .. } => Ok(serde_json::json!({"ok":true,"human_events":0})),
            other => panic!("unexpected bridge op {other:?}"),
        });
        let caller = EntityId::new();
        let (session_id, session) = open_session_ready(&fixture, caller);
        let token = lease_for(&fixture, session_id, caller);
        let observed = fixture.backend.capture(&session, None, false);
        let sealed = sealed_observation(&observed);
        assert_eq!(
            sealed.fingerprint_kind, KIND_NODE_TREE_PREFIX,
            "a bounded traversal is never labelled as the whole hierarchy"
        );
        let scope = sealed.fingerprint_scope.clone().expect("scope");
        assert!(scope.truncated);
        assert_eq!(scope.truncation_reason.as_deref(), Some("node_budget"));
        assert_eq!(
            observed.normalized["observation"]["fingerprint_kind"],
            serde_json::json!(KIND_NODE_TREE_PREFIX)
        );

        let outcome = fixture.backend.inject(
            &session,
            token,
            &sealed,
            vec![MobileActionInput::Tap { x: 10.0, y: 10.0 }],
            false,
        );
        assert_eq!(outcome.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            outcome.normalized["reason"],
            "stale_observation_fingerprint_scope"
        );
        assert!(
            !fixture
                .bridge
                .seen()
                .iter()
                .any(|op| matches!(op, BridgeOp::InputBatch { .. })),
            "re-scoped evidence cannot be compared as unchanged"
        );
    }

    /// A device that answers with a digest this node cannot read, or for a
    /// scope it did not request, has not produced hierarchy evidence: nothing
    /// is bound and the absence says why.
    #[test]
    fn unusable_fingerprint_answer_is_a_recorded_absence() {
        let shapes = [
            serde_json::json!({"fingerprint": "pixels-please", "algorithm": crate::fingerprint::FINGERPRINT_ALGORITHM}),
            serde_json::json!({"fingerprint": hierarchy_digest('a'), "algorithm": "sha256/2"}),
            serde_json::json!({"fingerprint": hierarchy_digest('a'), "algorithm": crate::fingerprint::FINGERPRINT_ALGORITHM, "region": [0, 0, 8, 8]}),
        ];
        for patch in shapes {
            let patch_for_responder = patch.clone();
            let fixture = fixture(move |op| match op {
                BridgeOp::Probe => Ok(probe_value()),
                BridgeOp::ScreenCapture { .. } => Ok(capture_value(64, 48)),
                BridgeOp::NodeTreeFingerprint { .. } => {
                    let mut value =
                        fingerprint_value(&hierarchy_digest('a'), "bounds_intersect_region", "", 12);
                    for (key, replacement) in patch_for_responder.as_object().expect("object patch") {
                        value[key] = replacement.clone();
                    }
                    Ok(value)
                }
                other => panic!("unexpected bridge op {other:?}"),
            });
            let caller = EntityId::new();
            let (_session_id, session) = open_session_ready(&fixture, caller);
            let observed = fixture.backend.capture(&session, None, false);
            let sealed = sealed_observation(&observed);
            assert_eq!(sealed.fingerprint_kind, KIND_PIXELS_ONLY, "{patch}");
            assert!(
                sealed.frame_fingerprint.starts_with("pixels:"),
                "no unreadable digest may become a frame fingerprint: {patch}"
            );
            let absence = sealed.fingerprint_absent.expect("absence");
            assert_eq!(absence.reason, "protocol_violation", "{patch}");
        }
    }
}
