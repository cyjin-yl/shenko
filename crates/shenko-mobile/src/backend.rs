//! Shared live backend for the six Android native kinds.
//!
//! Mirrors the desktop Computer Use backend shape (one Cordis-registered kind
//! per handler over one shared `AndroidBackend`) with the §24.3 display
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
use shenko_subject::NativeTaskInvocation;
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

/// Behavioral contract of the whole plugin set; sealed into every
/// operation's contract ref. Any semantic change = new digest.
const CONTRACT: &[u8] = b"shenko.mobile/v1: on-device bridge over real granted mechanisms (root su argv, Shizuku binder, Dhizuku device-owner, accessibility, normal perms); bridge token authenticates the bridge service incarnation only and never elevates authority; one display domain has one writer epoch and takeover revokes it; observation revision is per-session monotonic; appdb snapshots keep located/readable/recovered/consistent/plaintext/structure/semantics as separate facts and never claim decrypted or semantically complete content; transport ACK is not an application effect; nothing replays";

const MAX_ACTIONS: usize = 64;
const MAX_TEXT_CHARS: usize = 4096;
/// Total capture bound, matching the desktop provider.
const MAX_REGION_PX: i64 = 32 * 1024 * 1024;
const MAX_PNG_BYTES: usize = 16 * 1024 * 1024;
const MAX_SHELL_ARGV: usize = 64;
const MAX_SHELL_ARG_BYTES: usize = 4096;
const MAX_SHELL_ENV: usize = 32;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 64 * 1024 * 1024;
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

#[derive(Debug, Clone, Copy)]
struct LeaseHolder {
    caller: EntityId,
    session: EntityId,
    token: EntityId,
    acquired_at_ms: i64,
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
    pub fingerprint_kind: String,
    pub pixel_hash: String,
    pub raw_frame_ref: ObjectRef,
}

/// Normalized per-action input receipt.
#[derive(Debug, Clone, Serialize)]
pub struct MobileActionReceipt {
    pub index: usize,
    pub kind: String,
    /// "acked" = the device accepted the event; never an app effect.
    pub transport: &'static str,
    pub authority_used: String,
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
                let parsed: Option<CapabilityState> =
                    serde_json::from_value(value.clone()).ok();
                if let Some(parsed) = parsed {
                    state.capabilities.insert(kind.clone(), parsed);
                }
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
        let display = probe
            .get("display")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let width = display.get("width_px").and_then(|v| v.as_i64()).unwrap_or(0);
        let height = display.get("height_px").and_then(|v| v.as_i64()).unwrap_or(0);
        if width <= 0 || height <= 0 {
            return DriveOutcome::unavailable(
                "display_unknown",
                "bridge probe returned no usable display metrics".to_string(),
            );
        }
        let display_id = display_id.unwrap_or(0) as i32;
        let session = Arc::new(MobileSession {
            session_id: EntityId::new(),
            display_id,
            opened_by: caller,
            device_binding: self.device_binding().unwrap_or(DeviceBinding {
                digest: "unbound".to_string(),
                method: "unbound".to_string(),
                bound_at_ms: now_ms(),
            }),
            width_px: width as i32,
            height_px: height as i32,
            density_dpi: display
                .get("density_dpi")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            rotation_deg: display
                .get("rotation_deg")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            external_takeover_detection: probe
                .pointer("/authorities/accessibility/state")
                .and_then(|v| v.as_str())
                == Some("available"),
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
            "device_binding": session.device_binding,
            "external_takeover_detection": session.external_takeover_detection,
            "probe": probe,
        });
        DriveOutcome::done(EffectStatus::None, ObservationLevel::PostconditionVerified, normalized)
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
        if let Ok(mut domain) = self.domain(session.display_id).lock() {
            if domain.holder.is_some_and(|holder| holder.session == session_id) {
                domain.holder = None;
            }
        }
        if let Ok(mut state) = self.state() {
            let _ = state.sessions.remove(&session_id);
        }
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            serde_json::json!({"result":"closed","session_id":session_id}),
        )
    }

    /// §24.3 lease semantics: one display domain, one live writer epoch.
    /// Takeover revokes the previous epoch; late machine input carrying the
    /// old token or an older epoch is rejected before any event is sent.
    pub(crate) fn lease_op(
        self: &Arc<Self>,
        op: crate::handlers::LeaseOp,
        session_id: EntityId,
        token: Option<EntityId>,
        caller: EntityId,
    ) -> DriveOutcome {
        let Ok(session) = self.session(session_id) else {
            return DriveOutcome::rejected("session_unknown", serde_json::json!({}));
        };
        let domain = self.domain(session.display_id);
        let mut lease = match domain.lock() {
            Ok(lease) => lease,
            Err(_) => return DriveOutcome::rejected("backend_state_poisoned", serde_json::json!({})),
        };
        let mut takeover_revoked = false;
        match op {
            crate::handlers::LeaseOp::Acquire => {
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
            crate::handlers::LeaseOp::Release => {
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
            crate::handlers::LeaseOp::Takeover => {
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
    /// was acquired revokes the machine epoch exactly like a takeover. Only
    /// real when the accessibility feed is enabled; otherwise the session
    /// records detection=false and this check is skipped honestly.
    pub(crate) fn external_takeover_pending(
        &self,
        session: &MobileSession,
        lease_acquired_at_ms: i64,
    ) -> bool {
        if !session.external_takeover_detection {
            return false;
        }
        let op = BridgeOp::HumanActivityAfter {
            after_ms: lease_acquired_at_ms,
        };
        let Ok(result) = self.bridge.roundtrip(op) else {
            // Detection feed failure never blocks; it only disables the
            // takeover signal for this batch (stated in the observation).
            return false;
        };
        result.get("human_events").and_then(|v| v.as_u64()).unwrap_or(0) > 0
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
        let Some(png_b64) = result.get("png_b64").and_then(|v| v.as_str()) else {
            return DriveOutcome::bridge_failure(&BridgeError {
                kind: BridgeErrorKind::Internal,
                message: "bridge capture result missing png_b64".to_string(),
                retryable: false,
            });
        };
        let png = match B64.decode(png_b64.as_bytes()) {
            Ok(png) => png,
            Err(error) => {
                return DriveOutcome::bridge_failure(&BridgeError {
                    kind: BridgeErrorKind::Internal,
                    message: format!("capture png base64: {error}"),
                    retryable: false,
                })
            }
        };
        if png.len() > MAX_PNG_BYTES {
            return DriveOutcome::rejected(
                "capture_exceeds_png_bound",
                serde_json::json!({"png_bytes": png.len(), "bound": MAX_PNG_BYTES}),
            );
        }
        let (rgb, width, height) = match decode_png_rgb(&png) {
            Ok(decoded) => decoded,
            Err(reason) => return DriveOutcome::rejected(reason, serde_json::json!({})),
        };
        if i64::from(width) * i64::from(height) > MAX_REGION_PX {
            return DriveOutcome::rejected(
                "capture_exceeds_region_bound",
                serde_json::json!({"width": width, "height": height}),
            );
        }
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
        let (node_hash, fingerprint_kind) = match result.get("node_tree_hash").and_then(|v| v.as_str()) {
            Some(hash) => (hash.to_string(), "accessibility_node_tree"),
            None => (format!("pixels:{pixel_hash}"), "pixels_only"),
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
            captured_at_ms: result
                .get("captured_at_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(now_ms),
            lease_epoch_at_capture: lease_epoch,
            region: region.unwrap_or([0, 0, width as i32, height as i32]),
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
            frame_fingerprint: node_hash,
            fingerprint_kind: fingerprint_kind.to_string(),
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
            "capture_method": result.get("method").cloned().unwrap_or(serde_json::Value::Null),
        });
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            normalized,
        )
    }

    /// Re-verify a bound observation before any input is sent: a fresh
    /// capture must reproduce the same fingerprint, viewport and rotation.
    /// With `require_pixel_fresh` the exact pixel hash must match too.
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
        let Some(png_b64) = result.get("png_b64").and_then(|v| v.as_str()) else {
            return Err("capture_missing_png");
        };
        let png = B64.decode(png_b64.as_bytes()).map_err(|_| "capture_png_b64")?;
        let (rgb, width, height) = decode_png_rgb(&png).map_err(|_| "capture_png_decode")?;
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
        let node_hash = result
            .get("node_tree_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("pixels:{}", blake3::hash(&rgb).to_hex()));
        if node_hash != sealed.frame_fingerprint {
            return Err("frame_changed");
        }
        if require_pixel_fresh {
            let pixel_hash = blake3::hash(&rgb).to_hex().to_string();
            if pixel_hash != sealed.pixel_hash {
                return Err("pixels_changed");
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    /// §24.3: every event is delivered only after the bound observation
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
        if sealed.lease_epoch_at_capture != epoch {
            return DriveOutcome::rejected(
                "stale_lease_epoch",
                serde_json::json!({
                    "capture_epoch": sealed.lease_epoch_at_capture,
                    "current_epoch": epoch,
                }),
            );
        }
        if self.external_takeover_pending(session, holder.acquired_at_ms) {
            let new_epoch = self.revoke_for_external_takeover(session);
            return DriveOutcome::rejected(
                "external_takeover_revoked_lease",
                serde_json::json!({
                    "session_id": session.session_id,
                    "new_epoch": new_epoch,
                }),
            );
        }
        if let Err(reason) = self.observation_fresh(session, sealed, require_pixel_fresh) {
            return DriveOutcome::rejected(
                match reason {
                    "viewport_changed" => "stale_observation_viewport",
                    "display_rotated" => "stale_observation_rotation",
                    "frame_changed" => "stale_observation_frame",
                    "pixels_changed" => "stale_observation_pixels",
                    _ => "stale_observation_unverifiable",
                },
                serde_json::json!({"detail": reason}),
            );
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
        let receipts: Vec<MobileActionReceipt> = result
            .get("receipts")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| MobileActionReceipt {
                        index: item
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(index),
                        kind: item
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        transport: match item.get("acked").and_then(|v| v.as_bool()) {
                            Some(true) => "acked",
                            _ => "not_acked",
                        },
                        authority_used: item
                            .get("authority_used")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        observed_effect: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let acked = receipts.iter().filter(|r| r.transport == "acked").count();
        let normalized = serde_json::json!({
            "result": "injected",
            "session_id": session.session_id,
            "lease_epoch": epoch,
            "actions_total": receipts.len(),
            "actions_acked": acked,
            "receipts": receipts,
            "transport_note": "device ACK of injected events is transport delivery, never an application effect; dropped delivery stays unknown and nothing replays",
        });
        // Transport-level completion; app-level effects of the input are
        // Unknown until observed (§24.4).
        DriveOutcome::done(EffectStatus::Unknown, ObservationLevel::TransportWritten, normalized)
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
