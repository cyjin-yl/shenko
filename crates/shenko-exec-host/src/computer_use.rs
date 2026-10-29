//! Real X11 desktop Computer Use backend (SPEC §24.3/§24.4, A22/A23).
//!
//! Real mechanisms only: a session opens an actual X11 client connection, an
//! observation reads the server framebuffer through GetImage and binds stable
//! display/frame identity to a recomputed layout fingerprint plus a hash of
//! the actually captured pixels, and input is injected through the real XTEST
//! extension. The write lease is a per-display-domain state machine enforced
//! by this product; XInput2 raw events whose source device is not a known
//! XTEST slave count as external (typically human) input and revoke the
//! machine epoch exactly like an explicit takeover.
//!
//! Honest limits, by design:
//! - XTEST FakeInput ACK is transport delivery, never an application-level
//!   effect. Nothing is replayed; a lost connection leaves effects Unknown.
//! - The lease cannot stop a human or an unmanaged X client from typing; it
//!   only invalidates this product's stale controller epoch.
//! - Wayland, VNC/RDP, Android and macOS/Windows providers are typed
//!   unavailable, not fake-bound.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xinput::{self, XIEventMask},
        xproto::{
            self, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ImageFormat, KEY_PRESS_EVENT,
            KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, Keysym,
        },
        xtest,
    },
    rust_connection::RustConnection,
};

/// XIAllMasterDevices selects every raw master/slave feed on the domain.
const XI_ALL_MASTER_DEVICES: u16 = 1;
/// XIAllDevices enumerates the hierarchy for XTEST source detection.
const XI_ALL_DEVICES: u16 = 0;
const CONTRACT: &[u8] = b"shenko.computer_use/v1: real X11 XTEST input and GetImage observation; observation-bound delivery; lease revocable by takeover or non-XTEST raw input; transport ACK is not an application effect";
const MAX_ACTIONS: usize = 64;
const MAX_TEXT_CHARS: usize = 4096;
const MAX_REGION_PX: i64 = 33_554_432;
const MAX_OBSERVATION_AGE_MS: i64 = 300_000;
const KEY_SHIFT_L: Keysym = 0xFFE1;

// ---------------------------------------------------------------------------
// Frozen inputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionVerb {
    Open,
    Close,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionInput {
    /// "x11" selects the real X11 backend; "auto" behaves like "x11". Any
    /// other value resolves to a typed-unavailable backend, never a fake.
    backend: Option<String>,
    /// Explicit X11 display address (e.g. ":99"). Falls back to `$DISPLAY`;
    /// never guessed toward a human desktop.
    display: Option<String>,
    screen: Option<i32>,
    /// Defaults to "open". "close" requires `session_id`.
    op: Option<SessionVerb>,
    session_id: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionFrozen {
    backend_x11: bool,
    backend_name: String,
    display: Option<String>,
    screen: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveInput {
    session_id: EntityId,
    /// Region in root coordinates [x, y, w, h]; absent = whole root window.
    region: Option<[i32; 4]>,
}

#[derive(Debug, Clone, Serialize)]
struct ObserveFrozen {
    session_id: EntityId,
    region: Option<[i32; 4]>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LeaseOp {
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

#[derive(Debug, Clone, Serialize)]
struct LeaseFrozen {
    session_id: EntityId,
    op: LeaseOp,
    lease_token: Option<EntityId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PressKind {
    /// One press followed by one release.
    Tap,
    Down,
    Up,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ActionInput {
    PointerMove { x: i32, y: i32 },
    Button { button: u8, press: PressKind },
    Key { key: String, press: Option<PressKind> },
    Text { text: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectInput {
    session_id: EntityId,
    lease_token: EntityId,
    /// `blake3:<hex>` digest of the sealed DesktopObservation this batch is
    /// bound to; re-verified before any event is sent.
    observation_id: String,
    actions: Vec<ActionInput>,
    /// Also re-capture the observed region and require an exact pixel match.
    /// Default false: layout fingerprint re-verification always applies.
    require_pixel_fresh: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct InjectFrozen {
    session_id: EntityId,
    lease_token: EntityId,
    observation_id: String,
    actions: Vec<ActionInput>,
    require_pixel_fresh: bool,
}

// ---------------------------------------------------------------------------
// Durable observation record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct SessionInfo {
    session_id: EntityId,
    backend: &'static str,
    display: String,
    screen_num: usize,
    root_window: u32,
    width_px: u16,
    height_px: u16,
    width_mm: u16,
    height_mm: u16,
    dpi_x: f64,
    dpi_y: f64,
    depth: u8,
    xtest_version: Option<String>,
    xinput2_version: Option<String>,
    xtest_source_device_ids: Vec<u16>,
    domain_id: String,
    external_takeover_detection: bool,
}

#[derive(Debug, Clone, Serialize)]
struct WindowShape {
    window: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    map_state: u8,
}

/// The sealed durable observation; its digest is the `observation_id` that
/// later input must carry. Content-bound: identical bytes ⇒ identical id.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DesktopObservation {
    session_id: EntityId,
    domain_id: String,
    display: String,
    screen_num: usize,
    frame_id: u64,
    captured_at_ms: i64,
    lease_epoch_at_capture: u64,
    /// Captured rectangle in root coordinates [x, y, w, h].
    region: [i32; 4],
    viewport: [u16; 2],
    dpi_x: f64,
    dpi_y: f64,
    /// X11 has no compositor-level fractional scale; physical px == logical.
    scale_factor: f64,
    layout_fingerprint: String,
    pixel_hash: String,
    pixel_format: PixelFormatInfo,
    raw_pixels_ref: ObjectRef,
    png_preview_ref: ObjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PixelFormatInfo {
    depth: u8,
    bits_per_pixel: u8,
    byte_order: String,
    visual_id: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
}

#[derive(Debug, Clone, Serialize)]
struct ActionReceipt {
    index: usize,
    kind: String,
    /// "acked" = the X server accepted the request; never an app effect.
    transport: &'static str,
    observed_effect: Option<String>,
}

#[derive(Debug, Serialize)]
struct LeaseNormalized {
    lease_token: Option<EntityId>,
    epoch: u64,
    held_released: usize,
}

/// One operation's terminal/normalized view and evidence classification.
struct DriveOutcome {
    outcome: Option<OperationOutcome>,
    effect: EffectStatus,
    level: Option<ObservationLevel>,
    decode: DecodeStatus,
    normalized: serde_json::Value,
}

impl DriveOutcome {
    /// Rejected before any effect could leave the process.
    fn rejected(reason: &'static str, normalized: serde_json::Value) -> Self {
        let mut normalized = normalized;
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

    fn unavailable(reason: &'static str, detail: String) -> Self {
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

    fn done(effect: EffectStatus, level: ObservationLevel, normalized: serde_json::Value) -> Self {
        Self {
            outcome: Some(OperationOutcome::Succeeded),
            effect,
            level: Some(level),
            decode: DecodeStatus::Decoded,
            normalized,
        }
    }
}

// ---------------------------------------------------------------------------
// Live backend state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HeldInput {
    Key(u8),
    Button(u8),
}

/// Per-session mutable state (never shared across sessions).
struct SessionShared {
    closed: bool,
    next_frame: u64,
    /// Sent-event counts per operation for reconcile reporting.
    send_log: BTreeMap<EntityId, usize>,
    /// Keycodes remapped for unmapped text keysyms; restored on close.
    remapped: Vec<(u8, Vec<Keysym>)>,
    /// Cached keyboard mapping: flat rows of `keysyms_per_keycode`.
    keymap: Option<(u8, Vec<Keysym>)>,
    /// Cached Shift modifier keycode: Some(queried result, possibly None).
    shift_code_cache: Option<Option<u8>>,
}

/// One state machine per shared display domain: every session opened to the
/// same display/root shares this lease, epoch and held-input set, so a
/// sibling session can never become a second writer on one desktop.
struct DomainState {
    /// Monotonic domain epoch; takeover/release/detected-external bump it.
    lease_epoch: u64,
    lease: Option<DomainLease>,
    /// Machine-held inputs on this domain; released on takeover/release/close.
    held: BTreeSet<HeldInput>,
    /// Session ids currently open on this domain.
    session_ids: BTreeSet<EntityId>,
    watch: Option<JoinHandle<()>>,
    /// Stop flag for the watcher thread; set when the backend is dropped so
    /// the thread exits instead of blocking forever on poll.
    watch_stop: Arc<AtomicBool>,
}

struct DomainLease {
    holder_task: EntityId,
    token: EntityId,
}

struct X11Session {
    session_id: EntityId,
    owner_task: EntityId,
    conn: RustConnection,
    info: SessionInfo,
    screen: xproto::Screen,
    shared: Arc<Mutex<SessionShared>>,
    domain: Arc<Mutex<DomainState>>,
}

impl X11Session {
    fn locked(&self) -> Result<MutexGuard<'_, SessionShared>> {
        self.shared
            .lock()
            .map_err(|_| anyhow::anyhow!("session shared state poisoned"))
    }

    fn domain_locked(&self) -> Result<MutexGuard<'_, DomainState>> {
        self.domain
            .lock()
            .map_err(|_| anyhow::anyhow!("domain state poisoned"))
    }
}

impl Drop for X11Session {
    fn drop(&mut self) {
        // Session-scoped teardown only: the domain lease and held inputs
        // survive while a sibling session still shares this domain.
        if let Ok(mut shared) = self.shared.lock() {
            let remapped = std::mem::take(&mut shared.remapped);
            drop(shared);
            restore_keymap(&self.conn, &remapped);
        }
        // Remove this session from its domain; the last session leaving
        // drops the lease and releases machine-held inputs on the wire.
        if let Ok(mut domain) = self.domain_locked() {
            domain.session_ids.remove(&self.session_id);
            if domain.session_ids.is_empty() {
                domain.lease = None;
                domain.lease_epoch += 1;
                let held: Vec<HeldInput> =
                    std::mem::take(&mut domain.held).into_iter().collect();
                drop(domain);
                release_held(&self.conn, self.screen.root, &held);
            }
        }
    }
}

struct BackendState {
    sessions: BTreeMap<EntityId, Arc<X11Session>>,
    /// One shared lease state per display domain id (`x11/<display>/<root>`).
    domains: BTreeMap<String, Arc<Mutex<DomainState>>>,
}

/// Shared live state behind the four Cordis-registered handler kinds, so
/// lease authority and session identity are one truth across task kinds.
pub struct ComputerUseBackend {
    store: Arc<Store>,
    state: Mutex<BackendState>,
}

struct CapturedFrame {
    data: Vec<u8>,
    image: x11rb::image::Image<'static>,
    rect: [i32; 4],
    pixel_format: PixelFormatInfo,
    visual: xproto::Visualtype,
}

struct DriveCtx {
    operation_id: EntityId,
    task_id: EntityId,
}

struct SendResult {
    events_sent: usize,
    postcondition_verified: bool,
    kind: String,
    detail: Option<String>,
}

impl SendResult {
    fn receipt(self, index: usize) -> ActionReceipt {
        ActionReceipt {
            index,
            kind: self.kind,
            transport: "acked",
            observed_effect: if self.postcondition_verified {
                self.detail
            } else {
                None
            },
        }
    }
}

enum SendError {
    /// Rejected before any event for this action left the process.
    Rejected(&'static str),
    /// The connection failed during/after send; delivery is unprovable.
    Lost,
}

struct MappedKey {
    code: u8,
    /// Real modifier keycode for a shifted row (None = no shift needed);
    /// resolved through GetModifierMapping, never a guessed keycode.
    shift_code: Option<u8>,
}

impl ComputerUseBackend {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            state: Mutex::new(BackendState {
                sessions: BTreeMap::new(),
                domains: BTreeMap::new(),
            }),
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_media(&serde_json::to_vec(value)?, "application/json", schema)
    }

    fn seal_media(&self, data: &[u8], media: &str, schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(data, media, "computer-use-effects", schema, 1)
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
            .map_err(|_| anyhow::anyhow!("computer-use backend state poisoned"))
    }

    fn session(&self, session_id: EntityId) -> Result<Arc<X11Session>, &'static str> {
        let state = self.state().map_err(|_| "backend_state_poisoned")?;
        match state.sessions.get(&session_id) {
            Some(session) => {
                let closed = session.locked().map(|s| s.closed).unwrap_or(true);
                if closed {
                    Err("session_closed")
                } else {
                    Ok(Arc::clone(session))
                }
            }
            None => Err("session_unknown"),
        }
    }

    // ------------------------------------------------------------------
    // T1/E1/T2 drive: arm precedes any external effect; X11 work is blocking.
    // ------------------------------------------------------------------

    async fn drive<I, F>(
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
        I: Serialize + Send + 'static,
        F: FnOnce(&Arc<ComputerUseBackend>, &DriveCtx) -> DriveOutcome + Send + 'static,
    {
        let input_ref = self.seal(&frozen, frozen_schema)?;
        let bindings = ResourceBindings {
            provider: operation_name.into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "real_x11_bound_by_frozen_input".into(),
            extra: extra_binding,
        };
        let resource = self.seal(&bindings, "computer-use-binding")?;
        let acceptance_ref = self.seal(&acceptance, "computer-use-acceptance")?;
        let retry = self.seal(
            &"no automatic replay; armed XTEST input without conclusive evidence remains unknown",
            "computer-use-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: format!("builtin:{operation_name}"),
            },
            symbol: format!("{operation_name}.execute"),
            schema: SchemaRef {
                name: "computer-use-observation-v1".into(),
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
                plugin_id: computer_use_plugin_id(),
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
            task_id: invocation.task_invocation.task_id,
        };
        let backend = Arc::clone(self);
        let outcome = tokio::task::spawn_blocking(move || work(&backend, &ctx))
            .await
            .map_err(|error| anyhow::anyhow!("computer-use worker join: {error}"))?;

        let normalized_bytes = serde_json::to_vec(&outcome.normalized)?;
        let raw = self.seal_media(&normalized_bytes, "application/json", "computer-use-observation")?;
        let normalized_ref = Some(self.seal(&outcome.normalized, "computer-use-result")?);
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

    // ------------------------------------------------------------------
    // Session open/close (T1 already armed)
    // ------------------------------------------------------------------

    fn open_session(&self, frozen: &SessionFrozen, owner_task: EntityId) -> DriveOutcome {
        if !frozen.backend_x11 {
            return DriveOutcome::unavailable(
                "backend_unavailable",
                format!(
                    "no implemented computer-use backend {:?}; only x11 is real",
                    frozen.backend_name
                ),
            );
        }
        let display = frozen
            .display
            .clone()
            .filter(|d| !d.trim().is_empty())
            .or_else(|| std::env::var("DISPLAY").ok())
            .unwrap_or_default();
        if display.trim().is_empty() {
            return DriveOutcome::unavailable(
                "no_display_address",
                "no explicit display and $DISPLAY unset",
            );
        }
        let (conn, default_screen) = match RustConnection::connect(Some(display.as_str())) {
            Ok(pair) => pair,
            Err(error) => {
                return DriveOutcome::unavailable(connect_error_kind(&error), error.to_string())
            }
        };
        let setup = conn.setup();
        // Negative screen numbers are invalid input, not an alias for 0.
        if let Some(screen) = frozen.screen {
            if screen < 0 {
                return DriveOutcome::unavailable(
                    "screen_invalid",
                    format!("negative screen {screen}"),
                );
            }
        }
        let screen_index = usize::try_from(
            frozen.screen.unwrap_or(default_screen as i32),
        )
        .unwrap_or(usize::MAX);
        let Some(screen) = setup.roots.get(screen_index).cloned() else {
            return DriveOutcome::unavailable(
                "screen_invalid",
                format!("{} screens present", setup.roots.len()),
            );
        };
        // Real capability probing via extension presence + version replies.
        let xtest_version = match conn.extension_information(xtest::X11_EXTENSION_NAME) {
            Ok(Some(_)) => match xtest::get_version(&conn, 2, 2)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                Some(v) => Some(format!("{}.{}", v.major_version, v.minor_version)),
                None => Some("present_version_unknown".into()),
            },
            _ => None,
        };
        // XI raw `sourceid` is only defined for clients negotiating XI >= 2.1:
        // request 2.3 and enable detection only when the reply says >= 2.1.
        let negotiated_xi = match conn.extension_information(xinput::X11_EXTENSION_NAME) {
            Ok(Some(_)) => match xinput::xi_query_version(&conn, 2, 3)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                Some(v) => Some((v.major_version, v.minor_version)),
                None => None,
            },
            _ => None,
        };
        let xinput2_version =
            negotiated_xi.map(|(major, minor)| format!("{major}.{minor}"));
        let xi_raw_defined = negotiated_xi
            .map(|(major, minor)| major > 2 || (major == 2 && minor >= 1))
            .unwrap_or(false);
        let xtest_ids = xtest_device_ids(&conn);
        // The domain binds the display+root only; resource_id_base differs per
        // client connection and must never split the writer lease.
        // Different spellings of one server (":11", "unix:11", "localhost:11",
        // TCP v. unix) must collapse to the same lease domain: normalize the
        // authority to `unix:` for local transports and lowercase the host.
        let domain_id = format!(
            "x11/{}/root{:08x}",
            normalize_display_authority(&display),
            screen.root
        );
        let dpi_x = if screen.width_in_millimeters > 0 {
            f64::from(screen.width_in_pixels) * 25.4 / f64::from(screen.width_in_millimeters)
        } else {
            0.0
        };
        let dpi_y = if screen.height_in_millimeters > 0 {
            f64::from(screen.height_in_pixels) * 25.4 / f64::from(screen.height_in_millimeters)
        } else {
            0.0
        };
        let session_id = EntityId::new();
        // One DomainState per display domain across all sibling sessions.
        let domain = match self.state() {
            Ok(mut state) => match state.domains.get(&domain_id) {
                Some(existing) => Arc::clone(existing),
                None => {
                    let domain = Arc::new(Mutex::new(DomainState {
                        lease_epoch: 0,
                        lease: None,
                        held: BTreeSet::new(),
                        session_ids: BTreeSet::new(),
                        watch: None,
                        watch_stop: Arc::new(AtomicBool::new(false)),
                    }));
                    state
                        .domains
                        .insert(domain_id.clone(), Arc::clone(&domain));
                    domain
                }
            },
            Err(_) => return DriveOutcome::unavailable("backend_state_poisoned", String::new()),
        };
        // XI2 raw watcher, spawned at most once per domain: a raw event whose
        // source device is not a known XTEST slave is external input on this
        // domain and revokes the machine epoch. Detection requires XI >= 2.1
        // (sourceid defined) plus successful event selection. On Xvfb all
        // sources are XTEST so this is a no-op; explicit takeover is
        // unconditional either way.
        let mut detection = false;
        if xi_raw_defined {
            let already = domain
                .lock()
                .map(|locked| locked.watch.is_some())
                .unwrap_or(false);
            if !already {
                if let Ok((watch_conn, _)) = RustConnection::connect(Some(display.as_str())) {
                    // Raw touch events exist only from XI 2.2; a mask naming
                    // them fails the whole selection on an XI 2.1 server.
                    let touch = negotiated_xi
                        .map(|(major, minor)| major > 2 || (major == 2 && minor >= 2))
                        .unwrap_or(false);
                    let mut mask_bits = u32::from(XIEventMask::RAW_KEY_PRESS)
                        | u32::from(XIEventMask::RAW_KEY_RELEASE)
                        | u32::from(XIEventMask::RAW_BUTTON_PRESS)
                        | u32::from(XIEventMask::RAW_BUTTON_RELEASE)
                        | u32::from(XIEventMask::RAW_MOTION);
                    if touch {
                        mask_bits |= u32::from(XIEventMask::RAW_TOUCH_BEGIN)
                            | u32::from(XIEventMask::RAW_TOUCH_UPDATE)
                            | u32::from(XIEventMask::RAW_TOUCH_END);
                    }
                    let masks = [xinput::EventMask {
                        deviceid: XI_ALL_MASTER_DEVICES,
                        mask: vec![XIEventMask::from(mask_bits)],
                    }];
                    let selected =
                        xinput::xi_select_events(&watch_conn, screen.root, &masks)
                            .ok()
                            .and_then(|c| c.check().ok())
                            .is_some();
                    if selected {
                        detection = true;
                        let weak = Arc::downgrade(&domain);
                        let ids = xtest_ids.clone();
                        let root = screen.root;
                        let stop = domain
                            .lock()
                            .map(|locked| Arc::clone(&locked.watch_stop))
                            .unwrap_or_else(|_| Arc::new(AtomicBool::new(false)));
                        let handle = std::thread::spawn(move || {
                            raw_watch_loop(watch_conn, root, ids, weak, stop);
                        });
                        if let Ok(mut locked) = domain.lock() {
                            locked.watch = Some(handle);
                        }
                    }
                }
            } else {
                detection = true;
            }
        }
        let info = SessionInfo {
            session_id,
            backend: "x11",
            display: display.clone(),
            screen_num: screen_index,
            root_window: screen.root,
            width_px: screen.width_in_pixels,
            height_px: screen.height_in_pixels,
            width_mm: screen.width_in_millimeters,
            height_mm: screen.height_in_millimeters,
            dpi_x,
            dpi_y,
            depth: screen.root_depth,
            xtest_version,
            xinput2_version,
            xtest_source_device_ids: xtest_ids,
            domain_id,
            external_takeover_detection: detection,
        };
        let session = Arc::new(X11Session {
            session_id,
            owner_task,
            conn,
            info: info.clone(),
            screen,
            shared: Arc::new(Mutex::new(SessionShared {
                closed: false,
                next_frame: 1,
                send_log: BTreeMap::new(),
                remapped: vec![],
                keymap: None,
                shift_code_cache: None,
            })),
            domain: Arc::clone(&domain),
        });
        match self.state() {
            Ok(mut state) => {
                if let Ok(mut locked) = domain.lock() {
                    locked.session_ids.insert(session_id);
                }
                state.sessions.insert(session_id, session);
            }
            Err(_) => return DriveOutcome::unavailable("backend_state_poisoned", String::new()),
        }
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            serde_json::json!({
                "result": "opened",
                "session": info,
            }),
        )
    }

    fn close_session(&self, session_id: EntityId, caller_task: EntityId) -> DriveOutcome {
        let session = match self.session(session_id) {
            Ok(s) => s,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        if session.owner_task != caller_task {
            return DriveOutcome::rejected(
                "not_session_owner",
                serde_json::json!({"session_id": session_id}),
            );
        }
        // Session state: mark closed and take this session's remapped keys.
        let remapped = match session.locked() {
            Ok(mut shared) => {
                shared.closed = true;
                std::mem::take(&mut shared.remapped)
            }
            Err(_) => vec![],
        };
        // Domain state: leave the domain; revoke the lease + release held
        // inputs only when this was the last session on the shared domain or
        // the closing task is the current lease holder.
        let (held, revoked) = match session.domain_locked() {
            Ok(mut domain) => {
                domain.session_ids.remove(&session_id);
                let holder_gone = domain
                    .lease
                    .as_ref()
                    .is_some_and(|l| l.holder_task == caller_task);
                if domain.session_ids.is_empty() || holder_gone {
                    domain.lease = None;
                    domain.lease_epoch += 1;
                    (
                        std::mem::take(&mut domain.held).into_iter().collect::<Vec<_>>(),
                        true,
                    )
                } else {
                    (vec![], false)
                }
            }
            Err(_) => (vec![], false),
        };
        release_held(&session.conn, session.screen.root, &held);
        restore_keymap(&session.conn, &remapped);
        if let Ok(mut state) = self.state() {
            state.sessions.remove(&session_id);
        }
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            serde_json::json!({
                "result": "closed",
                "session_id": session_id,
                "held_released": held.len(),
                "lease_revoked": revoked,
            }),
        )
    }

    // ------------------------------------------------------------------
    // Observation
    // ------------------------------------------------------------------

    /// Ordered viewable/mapped top-level geometry + live root geometry.
    /// Window moves, resizes, stacking, unmapping or a RandR root resize all
    /// change the fingerprint, so any layout change invalidates observations.
    /// Fails when the root geometry cannot be read — never a silent hash.
    fn layout_fingerprint(
        conn: &RustConnection,
        root: u32,
    ) -> Result<(String, Vec<WindowShape>), &'static str> {
        let mut shapes = vec![];
        let mut note = String::new();
        // Live root geometry: a RandR resize leaves the window list unchanged
        // but changes the valid coordinate space — it must invalidate.
        let root_geo = xproto::get_geometry(conn, root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| (g.x, g.y, g.width, g.height))
            .ok_or("root_geometry_failed")?;
        match xproto::query_tree(conn, root).ok().and_then(|c| c.reply().ok()) {
            Some(tree) => {
                for child in &tree.children {
                    let attrs = xproto::get_window_attributes(conn, *child)
                        .ok()
                        .and_then(|c| c.reply().ok());
                    let geom = xproto::get_geometry(conn, *child)
                        .ok()
                        .and_then(|c| c.reply().ok());
                    match (attrs, geom) {
                        (Some(attrs), Some(geom)) => shapes.push(WindowShape {
                            window: *child,
                            x: geom.x,
                            y: geom.y,
                            width: geom.width,
                            height: geom.height,
                            border_width: geom.border_width,
                            map_state: u8::from(attrs.map_state),
                        }),
                        _ => note = format!("window:{child:x}"),
                    }
                }
            }
            None => note = "tree_failed".into(),
        }
        let bytes = serde_json::to_vec(&(root, &root_geo, &shapes, &note)).unwrap_or_default();
        Ok((hex_digest(&bytes), shapes))
    }

    fn capture_region(
        session: &X11Session,
        region: [i32; 4],
    ) -> Result<CapturedFrame, &'static str> {
        // Clamp against LIVE root geometry, not the open-time snapshot: a
        // RandR resize between open and capture would otherwise bound the
        // region by stale dimensions.
        let live_geo = xproto::get_geometry(&session.conn, session.screen.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .ok_or("root_geometry_query_failed")?;
        let root_w = i32::from(live_geo.width.min(i16::MAX as u16));
        let root_h = i32::from(live_geo.height.min(i16::MAX as u16));
        let [x, y, w, h] = region;
        let x = x.clamp(0, root_w);
        let y = y.clamp(0, root_h);
        let w = w.clamp(0, root_w - x);
        let h = h.clamp(0, root_h - y);
        if w <= 0 || h <= 0 {
            return Err("region_out_of_bounds");
        }
        let reply = xproto::get_image(
            &session.conn,
            ImageFormat::Z_PIXMAP,
            session.screen.root,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            !0,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .ok_or("get_image_failed")?;
        let visual_id = if reply.visual != 0 {
            reply.visual
        } else {
            session.screen.root_visual
        };
        let data = reply.data.clone();
        let image = x11rb::image::Image::get_from_reply(
            session.conn.setup(),
            w as u16,
            h as u16,
            reply,
        )
        .map_err(|_| "image_decode_failed")?
        .into_owned();
        let Some(visual) = find_visual(&session.screen, visual_id) else {
            return Err("visual_not_truecolor");
        };
        let Some(format) = session
            .conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == image.depth())
            .cloned()
        else {
            return Err("pixmap_format_unknown");
        };
        Ok(CapturedFrame {
            data,
            image,
            rect: [x, y, w, h],
            pixel_format: PixelFormatInfo {
                depth: image.depth(),
                bits_per_pixel: format.bits_per_pixel,
                byte_order: format!("{:?}", session.conn.setup().image_byte_order),
                visual_id,
                red_mask: visual.red_mask,
                green_mask: visual.green_mask,
                blue_mask: visual.blue_mask,
            },
            visual,
        })
    }

    fn observe(&self, frozen: &ObserveFrozen) -> DriveOutcome {
        let session = match self.session(frozen.session_id) {
            Ok(s) => s,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let region = frozen.region.unwrap_or([
            0,
            0,
            i32::from(session.screen.width_in_pixels),
            i32::from(session.screen.height_in_pixels),
        ]);
        let frame = match Self::capture_region(&session, region) {
            Ok(f) => f,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let (fingerprint, shapes) = match Self::layout_fingerprint(&session.conn, session.screen.root) {
            Ok(pair) => pair,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let pixel_hash = hex_digest(&frame.data);
        let rgb = match image_to_rgb8(&frame) {
            Ok(rgb) => rgb,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let png = match encode_png(
            u32::from(frame.image.width()),
            u32::from(frame.image.height()),
            &rgb,
        ) {
            Ok(png) => png,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let raw_ref = match self.seal_media(
            &frame.data,
            "application/x-x11-zpixmap",
            "computer-use-pixels",
        ) {
            Ok(r) => r,
            Err(_) => return DriveOutcome::unavailable("cas_unavailable", String::new()),
        };
        let png_ref = match self.seal_media(&png, "image/png", "computer-use-preview") {
            Ok(r) => r,
            Err(_) => return DriveOutcome::unavailable("cas_unavailable", String::new()),
        };
        let frame_id = match session.locked() {
            Ok(mut shared) => {
                let id = shared.next_frame;
                shared.next_frame += 1;
                id
            }
            Err(_) => return DriveOutcome::unavailable("backend_state_poisoned", String::new()),
        };
        let epoch = session
            .domain_locked()
            .map(|domain| domain.lease_epoch)
            .unwrap_or(0);
        let observation = DesktopObservation {
            session_id: frozen.session_id,
            domain_id: session.info.domain_id.clone(),
            display: session.info.display.clone(),
            screen_num: session.info.screen_num,
            frame_id,
            captured_at_ms: chrono::Utc::now().timestamp_millis(),
            lease_epoch_at_capture: epoch,
            region: frame.rect,
            viewport: [frame.image.width(), frame.image.height()],
            dpi_x: session.info.dpi_x,
            dpi_y: session.info.dpi_y,
            scale_factor: 1.0,
            layout_fingerprint: fingerprint,
            pixel_hash,
            pixel_format: frame.pixel_format.clone(),
            raw_pixels_ref: raw_ref,
            png_preview_ref: png_ref,
        };
        let observation_ref = match self.seal(&observation, "computer-use-observation-record") {
            Ok(r) => r,
            Err(_) => return DriveOutcome::unavailable("cas_unavailable", String::new()),
        };
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            serde_json::json!({
                "result": "observed",
                "observation_id": observation_ref.digest.0,
                "observation": observation,
                "top_level_windows": shapes,
            }),
        )
    }

    // ------------------------------------------------------------------
    // Lease state machine (one writer per shared display domain)
    // ------------------------------------------------------------------

    /// Revoke the current machine epoch on the shared domain. Bumping
    /// `lease_epoch` invalidates every bound token and observation epoch;
    /// held inputs are released on the real connection.
    fn revoke_locked(session: &X11Session, domain: &mut DomainState) -> (usize, u64) {
        domain.lease = None;
        domain.lease_epoch += 1;
        let epoch = domain.lease_epoch;
        let held: Vec<HeldInput> = std::mem::take(&mut domain.held).into_iter().collect();
        // The X calls run while holding `domain`; that is the point of the
        // lock: a concurrent injection action cannot slip events between the
        // epoch bump and the releases.
        release_held(&session.conn, session.screen.root, &held);
        (held.len(), epoch)
    }

    fn lease_op(&self, frozen: &LeaseFrozen, caller_task: EntityId) -> DriveOutcome {
        let session = match self.session(frozen.session_id) {
            Ok(s) => s,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let mut domain = match session.domain_locked() {
            Ok(s) => s,
            Err(_) => return DriveOutcome::unavailable("backend_state_poisoned", String::new()),
        };
        match frozen.op {
            LeaseOp::Acquire => {
                if let Some(lease) = &domain.lease {
                    if lease.holder_task == caller_task {
                        return DriveOutcome::done(
                            EffectStatus::None,
                            ObservationLevel::RemoteCompleted,
                            serde_json::json!({
                                "result": "held",
                                "lease": LeaseNormalized {
                                    lease_token: Some(lease.token),
                                    epoch: domain.lease_epoch,
                                    held_released: 0,
                                },
                            }),
                        );
                    }
                    return DriveOutcome::rejected(
                        "lease_conflict",
                        serde_json::json!({
                            "lease": LeaseNormalized {
                                lease_token: None,
                                epoch: domain.lease_epoch,
                                held_released: 0,
                            },
                        }),
                    );
                }
                let token = EntityId::new();
                domain.lease = Some(DomainLease {
                    holder_task: caller_task,
                    token,
                });
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::RemoteCompleted,
                    serde_json::json!({
                        "result": "acquired",
                        "lease": LeaseNormalized {
                            lease_token: Some(token),
                            epoch: domain.lease_epoch,
                            held_released: 0,
                        },
                    }),
                )
            }
            LeaseOp::Release => {
                let owns = domain
                    .lease
                    .as_ref()
                    .is_some_and(|l| Some(l.token) == frozen.lease_token);
                if !owns {
                    return DriveOutcome::rejected(
                        "lease_token_mismatch",
                        serde_json::json!({
                            "lease": LeaseNormalized {
                                lease_token: None,
                                epoch: domain.lease_epoch,
                                held_released: 0,
                            },
                        }),
                    );
                }
                let (released, epoch) = Self::revoke_locked(&session, &mut domain);
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::RemoteCompleted,
                    serde_json::json!({
                        "result": "released",
                        "lease": LeaseNormalized {
                            lease_token: None,
                            epoch,
                            held_released: released,
                        },
                    }),
                )
            }
            LeaseOp::Takeover => {
                // Human/explicit takeover: revoke the old epoch so late input
                // from the previous controller is rejected, then grant the
                // caller a fresh lease. Never a no-op even without a holder.
                let (released, _) = Self::revoke_locked(&session, &mut domain);
                let token = EntityId::new();
                domain.lease = Some(DomainLease {
                    holder_task: caller_task,
                    token,
                });
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::RemoteCompleted,
                    serde_json::json!({
                        "result": "taken_over",
                        "lease": LeaseNormalized {
                            lease_token: Some(token),
                            epoch: domain.lease_epoch,
                            held_released: released,
                        },
                    }),
                )
            }
        }
    }

    // ------------------------------------------------------------------
    // Input injection (observation-bound, lease-gated, never replayed)
    // ------------------------------------------------------------------

    fn verify_observation(
        &self,
        session: &X11Session,
        observation_id: &str,
        require_pixel_fresh: bool,
    ) -> Result<DesktopObservation, &'static str> {
        let bytes = self
            .store
            .cas
            .read_object(&Digest(observation_id.to_string()))
            .map_err(|_| "observation_unknown")?;
        let observation: DesktopObservation =
            serde_json::from_slice(&bytes).map_err(|_| "observation_corrupt")?;
        if observation.session_id != session.session_id {
            return Err("observation_session_mismatch");
        }
        if observation.domain_id != session.info.domain_id {
            return Err("observation_domain_mismatch");
        }
        let now = chrono::Utc::now().timestamp_millis();
        if now.saturating_sub(observation.captured_at_ms) > MAX_OBSERVATION_AGE_MS {
            return Err("observation_expired");
        }
        // A takeover/release bumps the domain epoch; an observation captured
        // under an older epoch was made against coordinates the holder no
        // longer controls and must be re-taken.
        match session.domain_locked() {
            Ok(domain) => {
                if domain.lease_epoch != observation.lease_epoch_at_capture {
                    return Err("observation_stale_epoch");
                }
            }
            Err(_) => return Err("backend_state_poisoned"),
        }
        let (fingerprint, _) = Self::layout_fingerprint(&session.conn, session.screen.root)
            .map_err(|_| "layout_query_failed")?;
        if fingerprint != observation.layout_fingerprint {
            return Err("observation_stale_layout");
        }
        if require_pixel_fresh {
            let frame = Self::capture_region(session, observation.region)?;
            if hex_digest(&frame.data) != observation.pixel_hash {
                return Err("observation_stale_pixels");
            }
        }
        Ok(observation)
    }

    fn inject(&self, frozen: &InjectFrozen, ctx: &DriveCtx) -> DriveOutcome {
        let session = match self.session(frozen.session_id) {
            Ok(s) => s,
            Err(reason) => return DriveOutcome::rejected(reason, serde_json::json!({})),
        };
        // Lease check on the shared domain: the token must match the current
        // holder, and it is re-verified before EVERY action — the domain lock
        // is released between actions so an external-input watcher or an
        // explicit takeover can revoke the epoch mid-batch and stop the rest.
        let bound_epoch = match session.domain_locked() {
            Ok(domain) => match &domain.lease {
                Some(lease) if lease.token == frozen.lease_token => domain.lease_epoch,
                _ => {
                    return DriveOutcome::rejected(
                        if domain.lease.is_some() {
                            "lease_revoked"
                        } else {
                            "no_lease"
                        },
                        serde_json::json!({"epoch": domain.lease_epoch}),
                    );
                }
            },
            Err(_) => return DriveOutcome::unavailable("backend_state_poisoned", String::new()),
        };
        if let Err(reason) =
            self.verify_observation(&session, &frozen.observation_id, frozen.require_pixel_fresh)
        {
            return DriveOutcome::rejected(reason, serde_json::json!({"epoch": bound_epoch}));
        }
        if let Ok(mut shared) = session.locked() {
            shared.send_log.insert(ctx.operation_id, 0);
        }
        let mut receipts: Vec<ActionReceipt> = vec![];
        let mut sent = 0usize;
        let mut unverifiable = 0usize;
        let mut lost = false;
        let mut failure: Option<&'static str> = None;
        for (index, action) in frozen.actions.iter().enumerate() {
            // Re-validate the lease under the domain lock before each event:
            // external input or an explicit takeover bumps the epoch and
            // invalidates the token mid-batch, not only between batches.
            let result = {
                let mut domain = match session.domain_locked() {
                    Ok(d) => d,
                    Err(_) => {
                        failure = Some("backend_state_poisoned");
                        break;
                    }
                };
                match &domain.lease {
                    Some(lease)
                        if lease.token == frozen.lease_token
                            && domain.lease_epoch == bound_epoch => {}
                    _ => {
                        failure = Some("lease_revoked");
                        break;
                    }
                }
                let mut shared = match session.locked() {
                    Ok(s) => s,
                    Err(_) => {
                        failure = Some("backend_state_poisoned");
                        break;
                    }
                };
                send_action(&session, &mut domain, &mut shared, action)
            };
            match result {
                Ok(receipt) => {
                    sent += receipt.events_sent;
                    if !receipt.postcondition_verified {
                        unverifiable += 1;
                    }
                    receipts.push(receipt.receipt(index));
                }
                Err(SendError::Rejected(reason)) => {
                    receipts.push(ActionReceipt {
                        index,
                        kind: action_kind(action).into(),
                        transport: "rejected",
                        observed_effect: Some(reason.into()),
                    });
                    failure = Some(reason);
                    break;
                }
                Err(SendError::Lost) => {
                    lost = true;
                    receipts.push(ActionReceipt {
                        index,
                        kind: action_kind(action).into(),
                        transport: "lost",
                        observed_effect: None,
                    });
                    break;
                }
            }
            if let Ok(mut shared) = session.locked() {
                shared.send_log.insert(ctx.operation_id, sent);
            }
        }
        let pointer_post = xproto::query_pointer(&session.conn, session.screen.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|p| [p.root_x, p.root_y]);
        if let Ok(mut shared) = session.locked() {
            shared.send_log.insert(ctx.operation_id, sent);
        }
        let base = serde_json::json!({
            "lease_epoch": bound_epoch,
            "actions": receipts,
            "pointer_post": pointer_post,
            "sent_events": sent,
        });
        if lost {
            // Delivered-or-not is unprovable per action once the connection
            // died mid-batch: the whole operation stays Unknown, never retried.
            let mut n = base;
            n["result"] = serde_json::json!("incomplete");
            n["reason"] = serde_json::json!("transport_lost");
            return DriveOutcome {
                outcome: Some(OperationOutcome::Interrupted),
                effect: EffectStatus::Unknown,
                level: None,
                decode: DecodeStatus::Uninterpreted,
                normalized: n,
            };
        }
        if let Some(reason) = failure {
            // Events already left the process before the rejection: None is a
            // lie — the batch is a partial effect with a failed outcome.
            if sent > 0 {
                let mut n = base;
                n["result"] = serde_json::json!("rejected");
                n["reason"] = serde_json::json!(reason);
                return DriveOutcome {
                    outcome: Some(OperationOutcome::Failed),
                    effect: EffectStatus::Partial,
                    level: Some(ObservationLevel::RemoteCompleted),
                    decode: DecodeStatus::Decoded,
                    normalized: n,
                };
            }
            return DriveOutcome::rejected(reason, base);
        }
        // Transport ACK is not an application effect: a batch containing any
        // action without a provable postcondition resolves Partial.
        let effect = if unverifiable == 0 && sent > 0 {
            EffectStatus::Complete
        } else if sent > 0 {
            EffectStatus::Partial
        } else {
            EffectStatus::None
        };
        let mut n = base;
        n["result"] = serde_json::json!("sent");
        DriveOutcome::done(
            effect,
            if unverifiable == 0 && sent > 0 {
                ObservationLevel::PostconditionVerified
            } else {
                ObservationLevel::RemoteCompleted
            },
            n,
        )
    }

    // ------------------------------------------------------------------
    // Reconcile: recovery reports in-memory progress honestly; no replay.
    // ------------------------------------------------------------------

    fn reconcile_input(
        &self,
        operation_id: EntityId,
    ) -> (Option<OperationOutcome>, EffectStatus, serde_json::Value) {
        let mut sent: Option<usize> = None;
        let mut alive = false;
        if let Ok(state) = self.state() {
            for session in state.sessions.values() {
                if let Ok(shared) = session.locked() {
                    if let Some(count) = shared.send_log.get(&operation_id) {
                        sent = Some(*count);
                        alive = !shared.closed;
                    }
                }
            }
        }
        let note = serde_json::json!({
            "observation": "controller recovered without a conclusive XTEST result",
            "sent_events_before_recovery": sent,
            "session_alive_in_memory": alive,
            "process_stop_proven": false,
        });
        match sent {
            // The batch never reached the transport: interrupted, no effects.
            Some(0) => (Some(OperationOutcome::Interrupted), EffectStatus::None, note),
            _ => (None, EffectStatus::Unknown, note),
        }
    }
}

impl Drop for ComputerUseBackend {
    fn drop(&mut self) {
        // Signal every domain watcher to stop, then join each thread so the
        // backend never leaves a live watcher polling a dead state map.
        if let Ok(state) = self.state.lock() {
            for domain in state.domains.values() {
                if let Ok(mut locked) = domain.lock() {
                    locked.watch_stop.store(true, Ordering::Relaxed);
                    if let Some(watch) = locked.watch.take() {
                        drop(locked);
                        let _ = watch.join();
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Input send primitives (real XTEST wire)
// ---------------------------------------------------------------------------

fn send_action(
    session: &X11Session,
    domain: &mut DomainState,
    shared: &mut SessionShared,
    action: &ActionInput,
) -> Result<SendResult, SendError> {
    match action {
        ActionInput::PointerMove { x, y } => {
            let x = (*x).clamp(0, i32::from(i16::MAX)) as i16;
            let y = (*y).clamp(0, i32::from(i16::MAX)) as i16;
            fake_input(&session.conn, MOTION_NOTIFY_EVENT, 0, session.screen.root, x, y)?;
            // Provable postcondition: real pointer position after ACK.
            let post = xproto::query_pointer(&session.conn, session.screen.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .ok_or(SendError::Lost)?;
            Ok(SendResult {
                events_sent: 1,
                postcondition_verified: post.root_x == x && post.root_y == y,
                kind: "pointer_move".into(),
                detail: Some(format!("pointer={}x{}", post.root_x, post.root_y)),
            })
        }
        ActionInput::Button { button, press } => {
            if !(1..=7).contains(button) {
                return Err(SendError::Rejected("button_out_of_range"));
            }
            match press {
                PressKind::Tap => {
                    fake_input(&session.conn, BUTTON_PRESS_EVENT, *button, session.screen.root, 0, 0)?;
                    fake_input(&session.conn, BUTTON_RELEASE_EVENT, *button, session.screen.root, 0, 0)?;
                    Ok(SendResult {
                        events_sent: 2,
                        postcondition_verified: false,
                        kind: "button".into(),
                        detail: None,
                    })
                }
                PressKind::Down => {
                    fake_input(&session.conn, BUTTON_PRESS_EVENT, *button, session.screen.root, 0, 0)?;
                    domain.held.insert(HeldInput::Button(*button));
                    Ok(SendResult {
                        events_sent: 1,
                        postcondition_verified: false,
                        kind: "button_down".into(),
                        detail: None,
                    })
                }
                PressKind::Up => {
                    fake_input(&session.conn, BUTTON_RELEASE_EVENT, *button, session.screen.root, 0, 0)?;
                    domain.held.remove(&HeldInput::Button(*button));
                    Ok(SendResult {
                        events_sent: 1,
                        postcondition_verified: false,
                        kind: "button_up".into(),
                        detail: None,
                    })
                }
            }
        }
        ActionInput::Key { key, press } => {
            let keysym = parse_key_name(key).ok_or(SendError::Rejected("key_name_unknown"))?;
            let press = press.unwrap_or(PressKind::Tap);
            send_keysym(session, domain, shared, keysym, press, "key")
        }
        ActionInput::Text { text } => {
            let mut sent = 0usize;
            for ch in text.chars() {
                let Some(keysym) = char_to_keysym(ch) else {
                    return Err(SendError::Rejected("text_char_unmappable"));
                };
                let result =
                    send_keysym(session, domain, shared, keysym, PressKind::Tap, "text")?;
                sent += result.events_sent;
            }
            Ok(SendResult {
                events_sent: sent,
                postcondition_verified: false,
                kind: "text".into(),
                detail: None,
            })
        }
    }
}

/// Resolve a keysym to a real keycode (remapping a spare keycode when the
/// symbol is unmapped, as xdotool does) then send through XTEST.
fn send_keysym(
    session: &X11Session,
    domain: &mut DomainState,
    shared: &mut SessionShared,
    keysym: Keysym,
    press: PressKind,
    kind: &str,
) -> Result<SendResult, SendError> {
    let mapped = keysym_to_keycode(session, shared, keysym)?;
    let root = session.screen.root;
    let conn = &session.conn;
    let mut sent = 0usize;
    if let Some(shift_code) = mapped.shift_code {
        fake_input(conn, KEY_PRESS_EVENT, shift_code, root, 0, 0)?;
        sent += 1;
    }
    match press {
        PressKind::Tap => {
            fake_input(conn, KEY_PRESS_EVENT, mapped.code, root, 0, 0)?;
            fake_input(conn, KEY_RELEASE_EVENT, mapped.code, root, 0, 0)?;
            sent += 2;
        }
        PressKind::Down => {
            fake_input(conn, KEY_PRESS_EVENT, mapped.code, root, 0, 0)?;
            domain.held.insert(HeldInput::Key(mapped.code));
            sent += 1;
        }
        PressKind::Up => {
            fake_input(conn, KEY_RELEASE_EVENT, mapped.code, root, 0, 0)?;
            domain.held.remove(&HeldInput::Key(mapped.code));
            sent += 1;
        }
    }
    if let Some(shift_code) = mapped.shift_code {
        fake_input(conn, KEY_RELEASE_EVENT, shift_code, root, 0, 0)?;
        sent += 1;
    }
    Ok(SendResult {
        events_sent: sent,
        postcondition_verified: false,
        kind: kind.into(),
        detail: None,
    })
}

fn keysym_to_keycode(
    session: &X11Session,
    shared: &mut SessionShared,
    keysym: Keysym,
) -> Result<MappedKey, SendError> {
    let conn = &session.conn;
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    if shared.keymap.is_none() {
        let count = max.saturating_sub(min).saturating_add(1);
        let reply = xproto::get_keyboard_mapping(conn, min, count)
            .ok()
            .and_then(|c| c.reply().ok())
            .ok_or(SendError::Lost)?;
        shared.keymap = Some((reply.keysyms_per_keycode, reply.keysyms));
    }
    // Snapshot: the remap path below mutates `shared`, so no slice borrow may
    // outlive the row scan.
    let (per, keymap) = match shared.keymap.clone() {
        Some((per, map)) => (usize::from(per), map),
        None => return Err(SendError::Rejected("key_unmappable")),
    };
    if per == 0 {
        return Err(SendError::Rejected("key_unmappable"));
    }
    // The real Shift keycode comes from the server modifier mapping (row 0),
    // not a guessed constant: an unmapped Shift means the key cannot be
    // produced honestly and the action is rejected.
    if shared.shift_code_cache.is_none() {
        shared.shift_code_cache = Some(
            xproto::get_modifier_mapping(conn)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|reply| {
                    // First row = Shift modifier (8 rows: Shift..Mod5).
                    let per_mod = reply.keycodes.len() / 8;
                    reply.keycodes[..per_mod.max(1).min(reply.keycodes.len())]
                        .iter()
                        .copied()
                        .find(|c| *c != 0)
                }),
        );
    }
    for (index, row) in keymap.chunks(per).enumerate() {
        let code = min.saturating_add(index as u8);
        if code > max {
            break;
        }
        if row.first() == Some(&keysym) {
            return Ok(MappedKey { code, shift_code: None });
        }
        if row.get(1) == Some(&keysym) {
            let Some(code_mod) = shared.shift_code_cache.flatten() else {
                return Err(SendError::Rejected("shift_modifier_unmapped"));
            };
            return Ok(MappedKey { code, shift_code: Some(code_mod) });
        }
    }
    // Unmapped: find a spare keycode (all-zero row), remap it, restore later.
    for index in (0..keymap.len() / per).rev() {
        let start = index * per;
        let row = &keymap[start..start + per];
        if !row.is_empty() && row.iter().all(|k| *k == 0) {
            let code = min.saturating_add(index as u8);
            if code > max {
                continue;
            }
            let original = row.to_vec();
            let mut new_row = original.clone();
            new_row[0] = keysym;
            xproto::change_keyboard_mapping(conn, 1, code, per as u8, &new_row)
                .ok()
                .and_then(|c| c.check().ok())
                .ok_or(SendError::Lost)?;
            shared.remapped.push((code, original));
            if let Some((_, cache)) = shared.keymap.as_mut() {
                cache[start] = keysym;
            }
            // A remapped row binds the keysym unshifted at slot 0.
            return Ok(MappedKey { code, shift_code: None });
        }
    }
    Err(SendError::Rejected("key_unmappable"))
}

fn fake_input(
    conn: &RustConnection,
    kind: u8,
    detail: u8,
    root: u32,
    x: i16,
    y: i16,
) -> Result<(), SendError> {
    xtest::fake_input(conn, kind, detail, 0, root, x, y, 0)
        .ok()
        .and_then(|c| c.check().ok())
        .ok_or(SendError::Lost)
}

fn release_held(conn: &RustConnection, root: u32, held: &[HeldInput]) {
    for input in held {
        let (kind, detail) = match input {
            HeldInput::Key(code) => (KEY_RELEASE_EVENT, *code),
            HeldInput::Button(button) => (BUTTON_RELEASE_EVENT, *button),
        };
        let _ = xtest::fake_input(conn, kind, detail, 0, root, 0, 0, 0).map(|c| c.check());
    }
    let _ = conn.flush();
}

fn restore_keymap(conn: &RustConnection, remapped: &[(u8, Vec<Keysym>)]) {
    for (keycode, original) in remapped {
        let _ = xproto::change_keyboard_mapping(conn, 1, *keycode, original.len() as u8, original)
            .map(|c| c.check());
    }
    let _ = conn.flush();
}

fn char_to_keysym(ch: char) -> Option<Keysym> {
    match ch {
        '\n' => Some(0xFF0D), // Return
        '\t' => Some(0xFF09), // Tab
        ' ' => Some(0x0020),  // space
        c if (' '..='~').contains(&c) => Some(c as Keysym),
        c if c.is_ascii_control() => None,
        c => Some(0x0100_0000 | c as Keysym), // Unicode keysym convention
    }
}

fn parse_key_name(name: &str) -> Option<Keysym> {
    if let Some(rest) = name.strip_prefix("U+") {
        let cp = u32::from_str_radix(rest, 16).ok()?;
        return char::from_u32(cp).and_then(char_to_keysym);
    }
    if name.chars().count() == 1 {
        return name.chars().next().and_then(char_to_keysym);
    }
    let keysym = match name {
        "Return" | "Enter" => 0xFF0D,
        "Tab" => 0xFF09,
        "Escape" | "Esc" => 0xFF1B,
        "BackSpace" => 0xFF08,
        "Delete" => 0xFFFF,
        "Insert" => 0xFF63,
        "Home" => 0xFF50,
        "End" => 0xFF57,
        "Page_Up" | "PageUp" => 0xFF55,
        "Page_Down" | "PageDown" => 0xFF56,
        "Left" => 0xFF51,
        "Up" => 0xFF52,
        "Right" => 0xFF53,
        "Down" => 0xFF54,
        "space" => 0x0020,
        "Shift_L" => KEY_SHIFT_L,
        "Shift_R" => 0xFFE2,
        "Control_L" => 0xFFE3,
        "Control_R" => 0xFFE4,
        "Alt_L" => 0xFFE9,
        "Alt_R" => 0xFFEA,
        "Super_L" => 0xFFEB,
        "Super_R" => 0xFFEC,
        "Caps_Lock" => 0xFFE5,
        "F1" => 0xFFBE,
        "F2" => 0xFFBF,
        "F3" => 0xFFC0,
        "F4" => 0xFFC1,
        "F5" => 0xFFC2,
        "F6" => 0xFFC3,
        "F7" => 0xFFC4,
        "F8" => 0xFFC5,
        "F9" => 0xFFC6,
        "F10" => 0xFFC7,
        "F11" => 0xFFC8,
        "F12" => 0xFFC9,
        _ => return None,
    };
    Some(keysym)
}

fn action_kind(action: &ActionInput) -> &'static str {
    match action {
        ActionInput::PointerMove { .. } => "pointer_move",
        ActionInput::Button { .. } => "button",
        ActionInput::Key { .. } => "key",
        ActionInput::Text { .. } => "text",
    }
}

fn raw_watch_loop(
    conn: RustConnection,
    root: u32,
    xtest_ids: Vec<u16>,
    shared: Weak<Mutex<DomainState>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match conn.poll_for_event() {
            Ok(Some(event)) => {
                let source = match &event {
                    Event::XinputRawButtonPress(e) => Some(e.sourceid),
                    Event::XinputRawButtonRelease(e) => Some(e.sourceid),
                    Event::XinputRawKeyPress(e) => Some(e.sourceid),
                    Event::XinputRawKeyRelease(e) => Some(e.sourceid),
                    Event::XinputRawMotion(e) => Some(e.sourceid),
                    Event::XinputRawTouchBegin(e) => Some(e.sourceid),
                    Event::XinputRawTouchUpdate(e) => Some(e.sourceid),
                    Event::XinputRawTouchEnd(e) => Some(e.sourceid),
                    _ => None,
                };
                if let Some(source) = source {
                    if !xtest_ids.contains(&source) {
                        // Non-XTEST source device: external input. Revoke the
                        // machine epoch and release machine-held inputs.
                        if let Some(strong) = shared.upgrade() {
                            if let Ok(mut locked) = strong.lock() {
                                locked.lease = None;
                                locked.lease_epoch += 1;
                                let held: Vec<HeldInput> = std::mem::take(&mut locked.held).into_iter().collect();
                                drop(locked);
                                release_held(&conn, root, &held);
                            }
                        }
                    }
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(_) => return,
        }
    }
}

fn connect_error_kind(error: &x11rb::errors::ConnectError) -> &'static str {
    use x11rb::errors::ConnectError::*;
    match error {
        DisplayParsingError(_) => "display_address_invalid",
        InvalidScreen => "screen_invalid",
        SetupFailed(_) | SetupAuthenticate(_) => "display_auth_failed",
        IoError(_) | Incomplete { .. } => "display_unreachable",
        _ => "backend_error",
    }
}

/// Normalize a display string to a canonical authority+number so that
/// ":N", "unix:N", "localhost:N" and "hostname:N" spellings of one server
/// collapse; distinct servers stay distinct. Screen selection is excluded —
/// the root id in the key distinguishes screens.
fn normalize_display_authority(display: &str) -> String {
    let trimmed = display.trim();
    let (host, number) = match trimmed.rsplit_once(':') {
        Some((host, number)) => (host, number.split('.').next().unwrap_or(number)),
        None => ("", trimmed),
    };
    let canonical_host: String = if host.is_empty()
        || host.eq_ignore_ascii_case("unix")
        || host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host.starts_with('/') // unix path form "/tmp/launch-x:N"
        || host.contains("/local")
    {
        "unix".into()
    } else {
        host.to_ascii_lowercase()
    };
    format!("{canonical_host}:{number}")
}

fn hex_digest(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

fn find_visual(screen: &xproto::Screen, id: u32) -> Option<xproto::Visualtype> {
    for depth in &screen.allowed_depths {
        for visual in &depth.visuals {
            if visual.visual_id == id
                && (visual.class == xproto::VisualClass::TRUE_COLOR
                    || visual.class == xproto::VisualClass::DIRECT_COLOR)
            {
                return Some(visual.clone());
            }
        }
    }
    None
}

fn xtest_device_ids(conn: &RustConnection) -> Vec<u16> {
    let mut ids = vec![];
    if let Some(reply) = xinput::xi_query_device(conn, XI_ALL_DEVICES)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        for dev in &reply.infos {
            if String::from_utf8_lossy(&dev.name).contains("XTEST") {
                ids.push(dev.deviceid);
            }
        }
    }
    ids
}

fn image_to_rgb8(frame: &CapturedFrame) -> Result<Vec<u8>, &'static str> {
    let layout = x11rb::image::PixelLayout::from_visual_type(frame.visual.clone())
        .map_err(|_| "visual_not_truecolor")?;
    let mut rgb =
        Vec::with_capacity(usize::from(frame.image.width()) * usize::from(frame.image.height()) * 3);
    for y in 0..frame.image.height() {
        for x in 0..frame.image.width() {
            let (r, g, b) = layout.decode(frame.image.get_pixel(x, y));
            rgb.extend_from_slice(&[(r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8]);
        }
    }
    Ok(rgb)
}

fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| "png_header")?;
        writer.write_image_data(rgb).map_err(|_| "png_data")?;
    }
    Ok(png)
}

fn computer_use_plugin_id() -> EntityId {
    "5f8b2a1d-9e3c-4a7b-8c1d-2e4f5a6b7c8d"
        .parse()
        .expect("fixed computer-use plugin UUID")
}

// ---------------------------------------------------------------------------
// Handlers: one Cordis-registered kind each, sharing the live backend state.
// ---------------------------------------------------------------------------

fn descriptor_for(kind: &str, authority: &str, prompt: &str) -> NativeTaskDescriptor {
    NativeTaskDescriptor {
        kind: kind.into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: computer_use_plugin_id(),
            artifact_digest: Digest::blake3(CONTRACT),
            registry_revision: 0,
        },
        role_revision: "native-computer-use/1".into(),
        authority_refs: vec![authority.into()],
        prompt_section: Some(prompt.into()),
    }
}

const SESSION_PROMPT: &str = "computer.use.session opens/closes a real X11 desktop session {\"backend\":\"x11\"|\"auto\",\"display\":\":N\"|null,\"screen\":int|null,\"op\":\"open\"|\"close\",\"session_id\":uuid?}. Returns session_id, probed XTEST/XInput2 versions, display domain identity and DPI. Non-x11 backends and unreachable displays fail typed-unavailable; a human desktop is never guessed.";
const OBSERVE_PROMPT: &str = "computer.use.observe captures a real framebuffer region via X11 GetImage {\"session_id\":uuid,\"region\":[x,y,w,h]|null}. Returns observation_id binding stable session/display identity, frame_id, layout fingerprint, pixel hash, viewport and DPI. Coordinates are only valid while that observation re-verifies.";
const INPUT_PROMPT: &str = "computer.use.input injects through the real XTEST extension {\"session_id\":uuid,\"lease_token\":uuid,\"observation_id\":\"blake3:…\",\"actions\":[pointer_move|button|key|text],\"require_pixel_fresh\":bool?}. The observation re-verifies and the lease epoch must be live before any event; ACK is recorded separately from observed effects, dropped delivery stays unknown, nothing replays.";
const LEASE_PROMPT: &str = "computer.use.lease manages the single write lease of one shared display domain {\"session_id\":uuid,\"op\":\"acquire\"|\"release\"|\"takeover\",\"lease_token\":uuid?}. One domain has one writer; takeover revokes the old epoch so late machine input is rejected and held keys/buttons are released. Read-only observation never needs a lease.";

pub struct ComputerUseSessionHandler {
    backend: Arc<ComputerUseBackend>,
}
pub struct ComputerUseObserveHandler {
    backend: Arc<ComputerUseBackend>,
}
pub struct ComputerUseInputHandler {
    backend: Arc<ComputerUseBackend>,
}
pub struct ComputerUseLeaseHandler {
    backend: Arc<ComputerUseBackend>,
}

impl ComputerUseSessionHandler {
    pub fn new(backend: Arc<ComputerUseBackend>) -> Self {
        Self { backend }
    }
    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let parsed: SessionInput = serde_json::from_value(invocation.input.clone())?;
        match parsed.op.unwrap_or(SessionVerb::Open) {
            SessionVerb::Open => {
                let name = parsed.backend.clone().unwrap_or_else(|| "auto".into());
                ensure!(name.len() <= 64, "backend name too long");
                let frozen = SessionFrozen {
                    backend_x11: matches!(name.as_str(), "x11" | "auto"),
                    backend_name: name.clone(),
                    display: parsed.display.clone(),
                    screen: parsed.screen,
                };
                self.backend
                    .drive(
                        invocation,
                        "computer.use.session",
                        "computer-use-session",
                        frozen.clone(),
                        "computer-use-session-input",
                        serde_json::json!({"backend": name, "mechanism": "x11rb RustConnection"}),
                        "a real X11 connection is established and capability versions are probed, or a typed-unavailable result names the exact missing capability",
                        move |backend, ctx| backend.open_session(&frozen, ctx.task_id),
                    )
                    .await
            }
            SessionVerb::Close => {
                let Some(session_id) = parsed.session_id else {
                    return Err(anyhow::anyhow!("close requires session_id"));
                };
                self.backend
                    .drive(
                        invocation,
                        "computer.use.session",
                        "computer-use-session",
                        serde_json::json!({"op":"close","session_id":session_id}),
                        "computer-use-session-input",
                        serde_json::json!({"mechanism": "x11rb RustConnection close"}),
                        "the session's X11 connection is dropped and its lease epoch revoked",
                        move |backend, ctx| backend.close_session(session_id, ctx.task_id),
                    )
                    .await
            }
        }
    }
}

impl ComputerUseObserveHandler {
    pub fn new(backend: Arc<ComputerUseBackend>) -> Self {
        Self { backend }
    }
    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: ObserveInput = serde_json::from_value(invocation.input.clone())?;
        if let Some(r) = input.region {
            ensure!(
                r[2] > 0 && r[3] > 0 && i64::from(r[2]) * i64::from(r[3]) <= MAX_REGION_PX,
                "region must have positive area bounded at 32Mpx"
            );
        }
        let frozen = ObserveFrozen {
            session_id: input.session_id,
            region: input.region,
        };
        self.backend
            .drive(
                invocation,
                "computer.use.observe",
                "computer-use-observe",
                frozen.clone(),
                "computer-use-observe-input",
                serde_json::json!({"mechanism": "X11 GetImage ZPixmap + layout fingerprint"}),
                "raw pixels hash to the recorded pixel_hash and the recorded layout fingerprint re-computes over the live window tree",
                move |backend, _ctx| backend.observe(&frozen),
            )
            .await
    }
}

impl ComputerUseInputHandler {
    pub fn new(backend: Arc<ComputerUseBackend>) -> Self {
        Self { backend }
    }
    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: InjectInput = serde_json::from_value(invocation.input.clone())?;
        ensure!(
            !input.actions.is_empty() && input.actions.len() <= MAX_ACTIONS,
            "actions must be 1..=64"
        );
        for action in &input.actions {
            if let ActionInput::Text { text } = action {
                ensure!(
                    !text.is_empty() && text.chars().count() <= MAX_TEXT_CHARS,
                    "text must be 1..=4096 chars"
                );
            }
        }
        let frozen = InjectFrozen {
            session_id: input.session_id,
            lease_token: input.lease_token,
            observation_id: input.observation_id,
            actions: input.actions,
            require_pixel_fresh: input.require_pixel_fresh.unwrap_or(false),
        };
        self.backend
            .drive(
                invocation,
                "computer.use.input",
                "computer-use-input",
                frozen.clone(),
                "computer-use-input-input",
                serde_json::json!({"mechanism": "XTEST FakeInput; observation re-verify; lease epoch check"}),
                "every action is delivered only after the bound observation re-verifies and the lease epoch is live; unverified delivery is never claimed complete",
                move |backend, ctx| backend.inject(&frozen, ctx),
            )
            .await
    }
}

impl ComputerUseLeaseHandler {
    pub fn new(backend: Arc<ComputerUseBackend>) -> Self {
        Self { backend }
    }
    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: LeaseInput = serde_json::from_value(invocation.input.clone())?;
        let frozen = LeaseFrozen {
            session_id: input.session_id,
            op: input.op,
            lease_token: input.lease_token,
        };
        self.backend
            .drive(
                invocation,
                "computer.use.lease",
                "computer-use-lease",
                frozen.clone(),
                "computer-use-lease-input",
                serde_json::json!({"mechanism": "per-domain exclusive writer epoch"}),
                "one display domain has at most one live writer epoch; takeover invalidates the previous epoch and releases held inputs",
                move |backend, ctx| backend.lease_op(&frozen, ctx.task_id),
            )
            .await
    }
}

#[async_trait]
impl NativeTaskHandler for ComputerUseSessionHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "computer.use.session",
            "computer_use.x11.session/v1",
            SESSION_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        reconcile_any(&self.backend, task, operation_id, "computer.use.session").await
    }
}

#[async_trait]
impl NativeTaskHandler for ComputerUseObserveHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "computer.use.observe",
            "computer_use.x11.observe/v1",
            OBSERVE_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        reconcile_any(&self.backend, task, operation_id, "computer.use.observe").await
    }
}

#[async_trait]
impl NativeTaskHandler for ComputerUseInputHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "computer.use.input",
            "computer_use.x11.input/v1",
            INPUT_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        self.reconcile_input_op(task, operation_id).await
    }
}

#[async_trait]
impl NativeTaskHandler for ComputerUseLeaseHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "computer.use.lease",
            "computer_use.x11.lease/v1",
            LEASE_PROMPT,
        )
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.exec(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        reconcile_any(&self.backend, task, operation_id, "computer.use.lease").await
    }
}

impl ComputerUseInputHandler {
    async fn reconcile_input_op(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.backend.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == "computer.use.input",
            "original_operation_contract_mismatch"
        );
        if operation.state != OperationState::InFlight {
            return Ok(operation);
        }
        let attempt = self.backend.store.external_operation_attempt(operation_id)?;
        if attempt.dispatch_stage == AttemptDispatchStage::Ended {
            return Ok(operation);
        }
        let (outcome, effect, note) = self.backend.reconcile_input(operation_id);
        let raw = self.backend.seal(&note, "computer-use-recovery")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "computer.use.input.recovery".into(),
            source_instance_id: self.backend.store.capabilities.incarnation.to_string(),
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
        self.backend.store.record_external_evidence(
            operation_id,
            attempt.attempt_id,
            evidence,
            outcome,
            effect,
        )
    }
}

/// Shared reconcile for non-input kinds: an armed session/observe/lease op
/// without conclusive evidence stays Unknown rather than being replayed or
/// auto-resolved.
async fn reconcile_any(
    backend: &Arc<ComputerUseBackend>,
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
            "observation": "controller recovered without a conclusive result",
            "process_stop_proven": false,
        }),
        "computer-use-recovery",
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
