//! Real desktop Computer Use backend (docs/specs/desktop-runtime.md#spec-24-1, docs/specs/desktop-runtime.md#spec-24-3, docs/specs/desktop-runtime.md#spec-24-4, A22/A23).
//!
//! Real mechanisms only. The product layer (`ComputerUseBackend`) owns the
//! operation ledger, the durable observation record and the input writer
//! contract; every platform fact comes from a real provider:
//!
//! - `X11Provider` (this file): a session opens an actual X11 client
//!   connection, an observation reads the server framebuffer through
//!   GetImage and binds stable display/frame identity to a recomputed
//!   layout fingerprint plus a hash of the actually captured pixels, and
//!   input is injected through the real XTEST extension. XInput2 raw
//!   events whose source device is not a known XTEST slave count as
//!   external (typically human) input and revoke the machine epoch
//!   exactly like an explicit takeover.
//! - `WindowsDesktopProvider`: Win32 framebuffer capture and SendInput.
//! - `MacOsDesktopProvider`: CoreGraphics capture and CGEvent injection.
//!
//! The write lease is a per-display-domain state machine enforced by this
//! product; the lease algorithm is shared (`desktop::apply_lease_op`) so no
//! two providers can drift into different takeover semantics.
//!
//! Honest limits, by design:
//! - XTEST FakeInput ACK is transport delivery, never an application-level
//!   effect. Nothing is replayed; a lost connection leaves effects Unknown.
//! - The lease cannot stop a human or an unmanaged X client from typing; it
//!   only invalidates this product's stale controller epoch.
//! - A platform whose provider cannot reach a real display returns a typed
//!   unavailable result naming the missing capability, never a fake-bound
//!   session.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
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
use crate::desktop::{
    DesktopAction, DesktopActionReceipt, DesktopCapability, DesktopCapabilityFact,
    DesktopCloseOutcome, DesktopCoordinateTransform, DesktopDisplay, DesktopError,
    DesktopFrame, DesktopInjectOutcome, DesktopInjectRequest, DesktopLease, DesktopLeaseOp,
    DesktopLeaseOutcome, DesktopLeaseState, DesktopObservation, DesktopOpenRequest,
    DesktopPixelFormat, DesktopPress, DesktopProvider, DesktopReconcileOutcome,
    DesktopSession, DesktopSessionHandle, apply_lease_op, encode_png, hex_digest,
    HumanInputFanout, platform_default_provider,
};
use x11rb::{
    connection::{Connection, RequestConnection},
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
/// The operation contract every provider is bound to. It names the
/// provider-neutral guarantees only: the per-platform mechanism (X11
/// XTEST/GetImage, Win32 SendInput/BitBlt, CGEvent/CGDisplayCreateImage)
/// is a provider fact reported in each session's capabilities, never a
/// product-wide claim.
const CONTRACT: &[u8] = b"shenko.computer_use/v1: real platform capture and input injection through a compiled-in provider; observation-bound delivery; one write lease per interactive display domain, revocable by takeover or external input; transport ACK is not an application effect";
const MAX_ACTIONS: usize = 64;
const MAX_TEXT_CHARS: usize = 4096;
const MAX_REGION_PX: i64 = 33_554_432;
const MAX_OBSERVATION_AGE_MS: i64 = 300_000;
const KEY_SHIFT_L: Keysym = 0xFFE1;
/// Capability level recorded on every computer-use operation binding: a
/// real provider bound by frozen input, not a simulated surface.
const REAL_PROVIDER_CAPABILITY_LEVEL: &str = "real_desktop_provider_bound_by_frozen_input";
/// Retry contract text for the neutral product: input that left the process
/// without conclusive transport evidence is unknown, never replayed.
const NO_REPLAY_RETRY_CONTRACT: &str =
    "no automatic replay; input armed on a real provider without conclusive transport evidence remains unknown";

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
    /// "x11" | "windows" | "macos" select a real provider; "auto" resolves
    /// to this platform's native provider. Any other value resolves to a
    /// typed-unavailable backend, never a fake.
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
    /// The resolved provider id ("x11", "windows", "macos").
    backend_name: String,
    display: Option<String>,
    screen: Option<i32>,
    /// Set by the close verb: which session to close.
    session_id: Option<EntityId>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseInput {
    session_id: EntityId,
    op: DesktopLeaseOp,
    lease_token: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize)]
struct LeaseFrozen {
    session_id: EntityId,
    op: DesktopLeaseOp,
    lease_token: Option<EntityId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectInput {
    session_id: EntityId,
    lease_token: EntityId,
    /// `blake3:<hex>` digest of the sealed DesktopObservation this batch is
    /// bound to; re-verified before any event is sent.
    observation_id: String,
    actions: Vec<DesktopAction>,
    /// Also re-capture the observed region and require an exact pixel match.
    /// Default false: layout fingerprint re-verification always applies.
    require_pixel_fresh: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct InjectFrozen {
    session_id: EntityId,
    lease_token: EntityId,
    observation_id: String,
    actions: Vec<DesktopAction>,
    require_pixel_fresh: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VmVerb {
    Start,
    Stop,
    State,
    CommitReceipt,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VmInput {
    /// start/stop mutate the VM lifecycle; state and commit_receipt read.
    op: VmVerb,
    /// VM name this operation owns; only VMs named here are ever touched.
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct VmFrozen {
    op: VmVerb,
    name: String,
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

    /// Same as `unavailable` for provider-supplied (non-static) reasons.
    fn unavailable_owned(reason: &str, detail: &str) -> Self {
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

    /// Same as `rejected` for provider-supplied (non-static) reasons.
    fn rejected_owned(reason: &str, detail: &str) -> Self {
        Self {
            outcome: Some(OperationOutcome::Failed),
            effect: EffectStatus::None,
            level: Some(ObservationLevel::RemoteCompleted),
            decode: DecodeStatus::Decoded,
            normalized: serde_json::json!({
                "result": "rejected",
                "reason": reason,
                "detail": detail,
            }),
        }
    }
}

fn desktop_error_outcome(error: &DesktopError) -> DriveOutcome {
    match error {
        DesktopError::Unavailable { .. } => {
            DriveOutcome::unavailable_owned(error.reason(), error.detail())
        }
        DesktopError::Rejected { .. } => {
            DriveOutcome::rejected_owned(error.reason(), error.detail())
        }
        DesktopError::Lost { .. } => DriveOutcome {
            outcome: Some(OperationOutcome::Interrupted),
            effect: EffectStatus::Unknown,
            level: None,
            decode: DecodeStatus::Uninterpreted,
            normalized: serde_json::json!({
                "result": "incomplete",
                "reason": "transport_lost",
                "detail": error.detail(),
            }),
        },
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

/// Per-session mutable state (never shared across sessions). Small and
/// short-lived — this mutex is never held across an X11 round-trip or a
/// send boundary, so it can never block a takeover for the session.
struct SessionShared {
    next_frame: u64,
    next_revision: u64,
    /// Sent-event counts per operation for reconcile reporting.
    send_log: BTreeMap<EntityId, usize>,
}

/// One state machine per shared display domain: every session opened to the
/// same display/root shares this lease, epoch and held-input set, so a
/// sibling session can never become a second writer on one desktop.
struct DomainState {
    /// The display domain identity this state guards, in the shared
    /// `x11/{socket_endpoint_key}/root{root:08x}` spelling other providers
    /// (browser) bind to.
    domain_id: String,
    /// Where external-input detections are dispatched after the lease
    /// revoke + held-input release (handover fence first, notify second).
    fanout: Arc<HumanInputFanout>,
    /// The shared lease truth of this display domain. The algorithm that
    /// mutates it is `desktop::apply_lease_op`, so no provider can drift
    /// into a different takeover semantic.
    lease_state: DesktopLeaseState,
    /// Machine-held inputs on this domain; released on takeover/release/close.
    /// Entries are ONLY holds this product originated — a press repeated over
    /// a foreign (caller/human) hold is never adopted for cleanup.
    held: BTreeSet<HeldInput>,
    /// Keycodes remapped for unmapped text keysyms on this domain;
    /// server-global, so they live with the domain and are restored under
    /// the same revocation handover as held inputs. Each entry is
    /// (keycode, original_row, server_applied_row) — restore is verified
    /// against the live row so later foreign changes are not clobbered.
    remapped: Vec<(u8, Vec<Keysym>, Vec<Keysym>)>,
    /// Session ids currently open on this domain.
    session_ids: BTreeSet<EntityId>,
    watch: Option<JoinHandle<()>>,
    /// True only when the LIVE watcher negotiated XI >= 2.1 on its own
    /// connection AND its raw-event selection was ACKed — i.e. it can
    /// actually detect external input. Never inferred from another
    /// connection's negotiated version.
    watch_raw: bool,
    /// Stop flag for the watcher thread; set when the backend is dropped so
    /// the thread exits instead of blocking forever on poll.
    watch_stop: Arc<AtomicBool>,
}

struct X11Session {
    session_id: EntityId,
    /// Cheap, lock-free liveness: `session()` must answer a closed query
    /// without contending on a send-critical mutex.
    closed: AtomicBool,
    /// The admitted controller instance that opened this session.
    owner_instance: EntityId,
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

    /// Acquire the domain lock ONLY for one abortable send boundary: the
    /// lease token must still match and the epoch must still be the bound
    /// one. Watcher/external takeover revokes between any two events, never
    /// only between batches or actions.
    fn lease_guard(
        &self,
        token: EntityId,
        epoch: u64,
    ) -> Result<MutexGuard<'_, DomainState>, SendError> {
        let domain = self
            .domain
            .lock()
            .map_err(|_| SendError::Rejected("backend_state_poisoned"))?;
        if self.closed.load(Ordering::Relaxed) {
            return Err(SendError::Rejected("session_closed"));
        }
        match &domain.lease_state.holder {
            Some(lease)
                if lease.token == token
                    && lease.holder_session == self.session_id
                    && domain.lease_state.epoch == epoch => {
                Ok(domain)
            }
            _ => Err(SendError::Rejected("lease_revoked")),
        }
    }
}

impl Drop for X11Session {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        // Remove this session from its domain. The lease is revoked only
        // when the departing session was the WRITER (holder_session) or it
        // was the last session on the domain — a non-holding sibling's
        // teardown must never drop the active writer's epoch.
        if let Ok(mut domain) = self.domain_locked() {
            domain.session_ids.remove(&self.session_id);
            let holder_gone = domain
                .lease_state
                .holder
                .as_ref()
                .is_some_and(|l| l.holder_session == self.session_id);
            if domain.session_ids.is_empty() || holder_gone {
                domain.lease_state.holder = None;
                domain.lease_state.epoch += 1;
                let held: Vec<HeldInput> =
                    std::mem::take(&mut domain.held).into_iter().collect();
                let remapped = std::mem::take(&mut domain.remapped);
                // Release inside the domain critical section: the next lease
                // cannot emit input until teardown completes (handover fence).
                release_held(&self.conn, self.screen.root, &held);
                restore_keymap(&self.conn, &remapped);
            }
        }
    }
}

/// Live state of the X11 provider: its sessions and the display domains it
/// owns. One domain id is one shared focus/mouse/keyboard domain.
struct X11State {
    sessions: BTreeMap<EntityId, Arc<X11Session>>,
    /// One shared lease state per display domain id (`x11/<display>/<root>`).
    domains: BTreeMap<String, Arc<Mutex<DomainState>>>,
}

/// The real X11 desktop provider (docs/specs/desktop-runtime.md#spec-24-1, docs/specs/desktop-runtime.md#spec-24-3): a genuine X11
/// connection, genuine GetImage observation and genuine XTEST input.
pub struct X11Provider {
    state: Mutex<X11State>,
    /// Human-input observation fanout shared with the product backend so
    /// browser/mobile peers bound to a display domain get the same event.
    fanout: Arc<HumanInputFanout>,
}

/// One live desktop session owned by the product backend.
struct DesktopSessionEntry {
    provider: Arc<dyn DesktopProvider>,
    handle: DesktopSessionHandle,
}

/// Shared live state behind the Cordis-registered handler kinds, so lease
/// authority and session identity are one truth across task kinds AND
/// across platforms (docs/specs/desktop-runtime.md#spec-24-3).
pub struct ComputerUseBackend {
    store: Arc<Store>,
    providers: BTreeMap<String, Arc<dyn DesktopProvider>>,
    sessions: Mutex<BTreeMap<EntityId, DesktopSessionEntry>>,
    /// Human-input observations from display domains dispatch here (SPEC
    /// docs/specs/desktop-runtime.md#spec-24-3, docs/specs/browser.md#spec-24-6-2); the browser provider registers its observer.
    human_input: Arc<HumanInputFanout>,
    /// Original LayerFS on a qualified Linux VM (docs/specs/durable-state.md#spec-10-2-1): every
    /// Store operation runs through the pinned original SDK inside the
    /// guest; this adapter only drives the VM lifecycle.
    layerfs: Arc<crate::layerfs::LayerFsAdapter>,
}

struct CapturedFrame {
    data: Vec<u8>,
    image: x11rb::image::Image<'static>,
    rect: [i32; 4],
    image_width: u16,
    image_height: u16,
    pixel_format: DesktopPixelFormat,
    visual: xproto::Visualtype,
}

struct DriveCtx {
    operation_id: EntityId,
    /// The admitted controller identity stable across this agent instance's
    /// native tasks (CallContext.caller_instance_id); session ownership and
    /// lease holding bind this, not a finite task id.
    caller: EntityId,
    /// The durable task this operation belongs to; a committed cancel
    /// intent on it stops further input (docs/specs/durable-state.md#spec-10-6).
    task_id: EntityId,
}

struct SendResult {
    events_sent: usize,
    postcondition_verified: bool,
    kind: String,
    detail: Option<String>,
}

impl SendResult {
    fn receipt(self, index: usize) -> DesktopActionReceipt {
        DesktopActionReceipt {
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
    /// The product backend owns the operation ledger and the observation
    /// record; every platform fact comes from a registered provider.
    pub fn new(store: Arc<Store>) -> Self {
        let human_input = Arc::new(HumanInputFanout::new());
        let mut providers: BTreeMap<String, Arc<dyn DesktopProvider>> = BTreeMap::new();
        providers.insert(
            "x11".into(),
            Arc::new(X11Provider::with_fanout(Arc::clone(&human_input))),
        );
        #[cfg(target_os = "windows")]
        providers.insert(
            "windows".into(),
            Arc::new(crate::desktop_windows::WindowsDesktopProvider::with_fanout(
                Arc::clone(&human_input),
            )),
        );
        #[cfg(target_os = "macos")]
        providers.insert(
            "macos".into(),
            Arc::new(crate::desktop_macos::MacOsDesktopProvider::with_fanout(
                Arc::clone(&human_input),
            )),
        );
        Self {
            store,
            providers,
            sessions: Mutex::new(BTreeMap::new()),
            human_input,
            layerfs: Arc::new(crate::layerfs::LayerFsAdapter::new(
                crate::layerfs::LayerFsAdapterConfig::from_env(),
            )),
        }
    }

    /// Register a peer provider (browser, mobile) that reacts to human
    /// input on a shared display domain.
    pub fn register_human_input_observer(
        &self,
        observer: Arc<dyn crate::desktop::HumanInputObserver>,
    ) {
        self.human_input.register(observer);
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

    /// The real providers this build contains. "auto" resolves to the
    /// platform's native provider; a provider that is not compiled for this
    /// platform is reported as such, never silently replaced.
    fn provider_for(&self, name: &str) -> Result<Arc<dyn DesktopProvider>, DesktopError> {
        let resolved = match name {
            "auto" => platform_default_provider().to_string(),
            other => other.to_string(),
        };
        match self.providers.get(&resolved) {
            Some(provider) => Ok(Arc::clone(provider)),
            None => Err(DesktopError::unavailable(
                "backend_not_available_on_this_build",
                format!(
                    "no {resolved} provider is compiled into this build (available: {})",
                    self.providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    /// Real, probed capabilities of every registered provider — the
    /// user-facing discovery surface (docs/specs/desktop-runtime.md#spec-24-1).
    fn provider_capabilities(&self) -> Vec<serde_json::Value> {
        self.providers
            .values()
            .map(|provider| {
                serde_json::json!({
                    "provider": provider.provider_id(),
                    "capabilities": provider.capabilities(),
                })
            })
            .collect()
    }

    /// Capability discovery answer (docs/specs/desktop-runtime.md#spec-24-1, docs/specs/desktop-runtime.md#spec-24-2, docs/specs/durable-state.md#spec-10-2-1): the desktop
    /// providers this build really contains, the platform default for
    /// "auto", and the original-LayerFS VM path's own readiness. It never
    /// starts a VM, connects to a display or spawns the companion.
    fn capabilities_facts(&self, requested: Option<&str>) -> serde_json::Value {
        let probe_note = "declared; probed live at session open";
        let providers = match requested {
            Some(name) => match self.provider_for(name) {
                Ok(provider) => vec![serde_json::json!({
                    "provider": provider.provider_id(),
                    "capabilities": provider.capabilities(),
                    "probe": probe_note,
                })],
                Err(error) => vec![serde_json::json!({
                    "provider": name,
                    "available": false,
                    "reason": error.reason(),
                    "detail": error.detail(),
                })],
            },
            None => self
                .provider_capabilities()
                .into_iter()
                .map(|mut entry| {
                    entry["probe"] = serde_json::json!(probe_note);
                    entry
                })
                .collect(),
        };
        serde_json::json!({
            "result": "available",
            "platform_default": platform_default_provider(),
            "providers": providers,
            "layerfs_vm": self.layerfs.capabilities(),
        })
    }

    fn session_entry(
        &self,
        session_id: EntityId,
    ) -> Result<(Arc<dyn DesktopProvider>, DesktopSessionHandle), DesktopError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))?;
        match sessions.get(&session_id) {
            Some(entry) => {
                if entry.handle.session.is_closed() {
                    return Err(DesktopError::unavailable("session_closed", ""));
                }
                Ok((
                    Arc::clone(&entry.provider),
                    DesktopSessionHandle {
                        session_id: entry.handle.session_id,
                        session: Arc::clone(&entry.handle.session),
                        owner_instance: entry.handle.owner_instance,
                    },
                ))
            }
            None => Err(DesktopError::unavailable("session_unknown", "")),
        }
    }
}

impl X11Provider {
    pub fn new() -> Self {
        Self::with_fanout(Arc::new(HumanInputFanout::new()))
    }

    pub fn with_fanout(fanout: Arc<HumanInputFanout>) -> Self {
        Self {
            state: Mutex::new(X11State {
                sessions: BTreeMap::new(),
                domains: BTreeMap::new(),
            }),
            fanout,
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, X11State>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("computer-use backend state poisoned"))
    }

    fn session(&self, session_id: EntityId) -> Result<Arc<X11Session>, &'static str> {
        let state = self.state().map_err(|_| "backend_state_poisoned")?;
        match state.sessions.get(&session_id) {
            Some(session) => {
                // Lock-free liveness read: this path is on the takeover
                // chain and must never wait on a send-critical mutex.
                if session.closed.load(Ordering::Relaxed) {
                    Err("session_closed")
                } else {
                    Ok(Arc::clone(session))
                }
            }
            None => Err("session_unknown"),
        }
    }
}

impl ComputerUseBackend {
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
            capability_level: REAL_PROVIDER_CAPABILITY_LEVEL.into(),
            extra: extra_binding,
        };
        let resource = self.seal(&bindings, "computer-use-binding")?;
        let acceptance_ref = self.seal(&acceptance, "computer-use-acceptance")?;
        let retry = self.seal(&NO_REPLAY_RETRY_CONTRACT, "computer-use-retry")?;
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
            caller: invocation.context.caller_instance_id,
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
}

impl X11Provider {
    // ------------------------------------------------------------------
    // Session open/close — trait implementation of `DesktopProvider`.
    // ------------------------------------------------------------------

    /// Connect a dedicated watcher client, negotiate XI on THAT connection
    /// (a sibling session's negotiation does not transfer), select raw masks
    /// gated on the watcher connection's negotiated version, and spawn the
    /// loop bound to this domain's stop flag. Called WITH the domain lock
    /// held: the caller supplies the already-cloned stop flag.
    fn spawn_domain_watcher(
        domain: &Arc<Mutex<DomainState>>,
        display: &str,
        root: u32,
        endpoint: &str,
        xtest_ids: &[u16],
        stop: Arc<AtomicBool>,
    ) -> std::io::Result<(JoinHandle<()>, bool)> {
        let (watch_conn, _) = RustConnection::connect(Some(display))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "watcher connect failed"))?;
        // DNS or resolver fallback can change between the session and this
        // second connection. Never publish a watcher on another X server.
        if socket_endpoint_key(&watch_conn)? != endpoint
            || !watch_conn.setup().roots.iter().any(|screen| screen.root == root)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "watcher connected to a different display endpoint",
            ));
        }
        let negotiated = match watch_conn.extension_information(xinput::X11_EXTENSION_NAME) {
            Ok(Some(_)) => xinput::xi_query_version(&watch_conn, 2, 3)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|v| (v.major_version, v.minor_version)),
            _ => None,
        };
        // sourceid is only defined for clients negotiating XI >= 2.1 — on
        // older servers the watcher would misclassify, so refuse to run.
        let (major, minor) = negotiated
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "xi negotiation failed"))?;
        let raw_defined = major > 2 || (major == 2 && minor >= 1);
        if !raw_defined {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "xi < 2.1: raw sourceid undefined",
            ));
        }
        let touch = major > 2 || (major == 2 && minor >= 2);
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
        // Detection capability is real only if THIS connection's raw
        // selection was ACKed — a failed select means no watcher at all,
        // so callers report detection=false rather than thread-alive.
        xinput::xi_select_events(&watch_conn, root, &masks)
            .ok()
            .and_then(|c| c.check().ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "xi select failed"))?;
        let weak = Arc::downgrade(domain);
        let ids = xtest_ids.to_vec();
        Ok((
            std::thread::spawn(move || {
                raw_watch_loop(watch_conn, root, ids, weak, stop);
            }),
            true,
        ))
    }

    fn open_provider_session(&self, request: &DesktopOpenRequest, owner_instance: EntityId) -> Result<DesktopSessionHandle, DesktopError> {
        let display = request
            .display
            .clone()
            .filter(|d| !d.trim().is_empty())
            .or_else(|| std::env::var("DISPLAY").ok())
            .unwrap_or_default();
        if display.trim().is_empty() {
            return Err(DesktopError::unavailable(
                "no_display_address",
                "no explicit display and $DISPLAY unset".to_string(),
            ));
        };
        // Parse the display BEFORE connecting. The u16 `6000 + display`
        // bound applies ONLY when the resolver's address list actually
        // contains a TCP target; unix-only spellings such as `unix/:N`
        // never run that arithmetic, so large display numbers stay legal.
        let parsed = match x11rb::reexports::x11rb_protocol::parse_display::parse_display(
            Some(display.as_str()),
        ) {
            Ok(p) => p,
            Err(_) => {
                return Err(DesktopError::unavailable(
                    "display_address_invalid",
                    format!("unparseable display {display:?}"),
                ))
            }
        };
        // Mirror connect_addresses: TCP targets exist only for a non-local
        // host without protocol="unix", or for the localhost fallback when
        // the spelling is protocol-less and hostless.
        let wants_tcp = ((parsed.protocol.is_none()
            || parsed.protocol.as_deref() != Some("unix"))
            && !parsed.host.is_empty()
            && parsed.host != "unix")
            || (parsed.protocol.is_none() && parsed.host.is_empty());
        if parsed.display > 59535 && wants_tcp {
            return Err(DesktopError::unavailable(
                "display_address_invalid",
                format!("display {} exceeds resolver TCP range", parsed.display),
            ));
        }
        let (conn, default_screen) = match RustConnection::connect(Some(display.as_str())) {
            Ok(pair) => pair,
            Err(error) => {
                let kind = connect_error_kind(&error);
                return Err(DesktopError::unavailable(kind, error.to_string()));
            }
        };
        let setup = conn.setup();
        // Negative screen numbers are invalid input, not an alias for 0.
        if let Some(screen) = request.screen {
            if screen < 0 {
                return Err(DesktopError::unavailable(
                    "screen_invalid",
                    format!("negative screen {screen}"),
                ));
            }
        }
        let screen_index = usize::try_from(
            request.screen.unwrap_or(default_screen as i32),
        )
        .unwrap_or(usize::MAX);
        let Some(screen) = setup.roots.get(screen_index).cloned() else {
            return Err(DesktopError::unavailable(
                "screen_invalid",
                format!("{} screens present", setup.roots.len()),
            ));
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
        let xtest_ids = xtest_device_ids(&conn);
        // The domain binds the display+root only; resource_id_base differs
        // per client connection and must never split the writer lease. The
        // key is the endpoint THIS connection actually used — read from the
        // real socket, not the spelling: ":90" that fell back to TCP joins
        // the same domain as "localhost:90" via the connected peer address,
        // and TCP spellings that resolve to different peers keep distinct
        // domains. If the endpoint cannot be determined, refuse the open
        // rather than guessing a key that could merge distinct servers.
        let normalized_display = match socket_endpoint_key(&conn) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return Err(DesktopError::unavailable("display_endpoint_unknown", error.to_string()))
            }
        };
        let domain_id = format!("x11/{}/root{:08x}", normalized_display, screen.root);
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
                        domain_id: domain_id.clone(),
                        fanout: Arc::clone(&self.fanout),
                        lease_state: DesktopLeaseState::new(),
                        held: BTreeSet::new(),
                        remapped: vec![],
                        session_ids: BTreeSet::new(),
                        watch: None,
                        watch_raw: false,
                        watch_stop: Arc::new(AtomicBool::new(false)),
                    }));
                    state
                        .domains
                        .insert(domain_id.clone(), Arc::clone(&domain));
                    domain
                }
            },
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        };
        // Domain watcher, at most one LIVE thread per domain — creation,
        // liveness check and publication all happen inside the domain lock
        // so two concurrent opens cannot race a duplicate watcher, and a
        // dead thread is rebuilt rather than counted as live. The watcher
        // only exists when it negotiated XI >= 2.1 AND its raw selection
        // was ACKed on its OWN connection — external_takeover_detection is
        // that real result, never inferred from the session connection.
        let mut detection = false;
        match domain.lock() {
            Ok(mut locked) => {
                let live = locked
                    .watch
                    .as_ref()
                    .is_some_and(|handle| !handle.is_finished());
                if live {
                    // A live watcher reports the capability IT negotiated and
                    // selected — not this session connection's XI version.
                    detection = locked.watch_raw;
                } else {
                    // A finished thread's join returns at once, so it is
                    // safe to join under the domain lock — which also
                    // closes the check/spawn/publish critical section.
                    if let Some(old) = locked.watch.take() {
                        let _ = old.join();
                    }
                    locked.watch_raw = false;
                    let stop = Arc::clone(&locked.watch_stop);
                    match Self::spawn_domain_watcher(
                        &domain,
                        &display,
                        screen.root,
                        &normalized_display,
                        &xtest_ids,
                        stop,
                    ) {
                        Ok((handle, raw_ok)) => {
                            locked.watch = Some(handle);
                            locked.watch_raw = raw_ok;
                            detection = raw_ok;
                        }
                        Err(_) => detection = false,
                    }
                }
            }
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
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
            closed: AtomicBool::new(false),
            owner_instance,
            conn,
            info: info.clone(),
            screen,
            shared: Arc::new(Mutex::new(SessionShared {
                next_frame: 1,
                next_revision: 1,
                send_log: BTreeMap::new(),
            })),
            domain: Arc::clone(&domain),
        });
        match self.state() {
            Ok(mut state) => {
                if let Ok(mut locked) = domain.lock() {
                    locked.session_ids.insert(session_id);
                }
                state.sessions.insert(session_id, Arc::clone(&session));
            }
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        }
        Ok(DesktopSessionHandle::new(session_id, session, owner_instance))
    }

    fn close_session(&self, session_id: EntityId, caller_instance: EntityId) -> Result<DesktopCloseOutcome, DesktopError> {
        let session = match self.session(session_id) {
            Ok(s) => s,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        if session.owner_instance != caller_instance {
            return Err(DesktopError::rejected(
                "not_session_owner",
                format!("session {session_id} belongs to another controller"),
            ));
        }
        // Liveness does not contend with injection. Session lookup and a
        // concurrent takeover never wait for the current Text action.
        session.closed.store(true, Ordering::Relaxed);
        // Revoke only when this exact session held the writer lease, or the
        // last session leaves. A non-holding sibling can close without
        // invalidating an active writer from the same controller.
        let outcome = match session.domain_locked() {
            Ok(mut domain) => {
                domain.session_ids.remove(&session_id);
                let holder_gone = domain
                    .lease_state
                    .holder
                    .as_ref()
                    .is_some_and(|l| l.holder_session == session_id);
                if domain.session_ids.is_empty() || holder_gone {
                    let (released, _) = Self::revoke_locked(&session, &mut domain);
                    Ok(DesktopCloseOutcome {
                        held_released: released,
                        lease_revoked: true,
                    })
                } else {
                    Ok(DesktopCloseOutcome {
                        held_released: 0,
                        lease_revoked: false,
                    })
                }
            }
            Err(_) => Err(DesktopError::unavailable("backend_state_poisoned", "")),
        };
        if let Ok(mut state) = self.state() {
            state.sessions.remove(&session_id);
        }
        outcome
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
        // Read metadata BEFORE moving `image` into the frame.
        let image_depth = image.depth();
        let image_width = image.width();
        let image_height = image.height();
        let Some(visual) = find_visual(&session.screen, visual_id) else {
            return Err("visual_not_truecolor");
        };
        let Some(format) = session
            .conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == image_depth)
            .cloned()
        else {
            return Err("pixmap_format_unknown");
        };
        Ok(CapturedFrame {
            data,
            image,
            rect: [x, y, w, h],
            image_width,
            image_height,
            pixel_format: DesktopPixelFormat {
                name: "x11_zpixmap".into(),
                depth: image_depth,
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

    fn observe_session(&self, session_id: EntityId, region: Option<[i32; 4]>) -> Result<DesktopFrame, DesktopError> {
        let session = match self.session(session_id) {
            Ok(s) => s,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        // Close/capture fence: the domain lock is held from liveness check
        // through publication. `close` stores `closed` before touching the
        // domain, so a completed close always flips the flag before it can
        // block on us; if `closed` is still false at the post-capture check
        // then close has not even started and CANNOT complete while we hold
        // this lock — an observation is therefore never published for a
        // session whose close already returned.
        let domain_guard = match session.domain_locked() {
            Ok(domain) => domain,
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        };
        if session.closed.load(Ordering::Relaxed) {
            return Err(DesktopError::unavailable("session_closed", ""));
        }
        // The bound epoch is taken BEFORE reading pixels; the domain lock
        // already prevents any epoch change mid-capture, and the explicit
        // post-capture comparison keeps that contract visible.
        let epoch_before = domain_guard.lease_state.epoch;
        // Default region means the whole CURRENT root, not the open-time
        // snapshot: a RandR-grown root must be observed in full.
        let region = match region {
            Some(r) => r,
            None => {
                let live = xproto::get_geometry(&session.conn, session.screen.root)
                    .ok()
                    .and_then(|c| c.reply().ok());
                match live {
                    Some(g) => [
                        0,
                        0,
                        i32::from(g.width),
                        i32::from(g.height),
                    ],
                    None => {
                        return Err(DesktopError::unavailable(
                            "root_geometry_failed",
                            "",
                        ))
                    }
                }
            }
        };
        // Fingerprint BEFORE and AFTER the pixel read: a window move/resize
        // during capture cannot bind pre-change pixels to the post-change
        // fingerprint — epoch equality alone does not see pure layout churn
        // because no lease event is required for it.
        let (fingerprint_before, _) =
            match Self::layout_fingerprint(&session.conn, session.screen.root) {
                Ok(pair) => pair,
                Err(reason) => return Err(DesktopError::unavailable(reason, "")),
            };
        let frame = match Self::capture_region(&session, region) {
            Ok(f) => f,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        if domain_guard.lease_state.epoch != epoch_before {
            return Err(DesktopError::unavailable(
                "epoch_changed_during_capture",
                format!("epoch changed mid-capture on {}", session.info.domain_id),
            ));
        }
        // Final liveness fence before anything derived from this session is
        // published: close cannot have completed while we held the domain
        // lock, and a flag that IS set here means close began between the
        // earlier check and now — but still could not finish, so refusing is
        // the only honest answer either way.
        if session.closed.load(Ordering::Relaxed) {
            return Err(DesktopError::unavailable("session_closed", ""));
        }
        let (fingerprint, shapes) = match Self::layout_fingerprint(&session.conn, session.screen.root) {
            Ok(pair) => pair,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        if fingerprint != fingerprint_before {
            return Err(DesktopError::unavailable(
                "layout_changed_during_capture",
                format!("window layout changed mid-capture on {}", session.info.domain_id),
            ));
        }
        let rgb = match image_to_rgb8(&frame) {
            Ok(rgb) => rgb,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        let (frame_id, revision) = match session.locked() {
            Ok(mut shared) => {
                shared.next_frame += 1;
                shared.next_revision += 1;
                // Both counters start at 1 and are monotonic per session, the
                // same ordering the Windows/macOS providers report, so a
                // consumer can compare revisions across providers.
                (shared.next_frame, shared.next_revision)
            }
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        };
        Ok(DesktopFrame {
            region: frame.rect,
            width: u32::from(frame.image_width),
            height: u32::from(frame.image_height),
            raw_pixels: frame.data,
            rgb,
            pixel_format: frame.pixel_format.clone(),
            layout_fingerprint: fingerprint,
            dpi_x: session.info.dpi_x,
            dpi_y: session.info.dpi_y,
            scale_factor: 1.0,
            frame_id,
            revision,
            lease_epoch: epoch_before,
            screen_num: session.info.screen_num,
            detail: serde_json::json!({
                "top_level_windows": shapes,
            }),
        })
    }

    // ------------------------------------------------------------------
    // Lease state machine (one writer per shared display domain)
    // ------------------------------------------------------------------

    /// Revoke the current machine epoch on the shared domain. Bumping
    /// `lease_epoch` invalidates every bound token and observation epoch;
    /// held inputs are released on the real connection.
    /// Release provider-side held inputs and remapped rows without touching
    /// the epoch. The caller is responsible for the lease-state mutation.
    fn release_provider_state(session: &X11Session, domain: &mut DomainState) -> usize {
        let held: Vec<HeldInput> = std::mem::take(&mut domain.held).into_iter().collect();
        // The X calls run while holding `domain`; that is the point of the
        // lock: a concurrent injection action cannot slip events between the
        // epoch bump and the releases.
        release_held(&session.conn, session.screen.root, &held);
        let remapped = std::mem::take(&mut domain.remapped);
        restore_keymap(&session.conn, &remapped);
        held.len()
    }

    /// Revoke the current machine epoch on the shared domain. Bumping
    /// `lease_epoch` invalidates every bound token and observation epoch;
    /// held inputs are released on the real connection.
    fn revoke_locked(session: &X11Session, domain: &mut DomainState) -> (usize, u64) {
        domain.lease_state.holder = None;
        domain.lease_state.epoch += 1;
        let epoch = domain.lease_state.epoch;
        (Self::release_provider_state(session, domain), epoch)
    }

    fn lease_op_session(&self, session_id: EntityId, op: DesktopLeaseOp, token: Option<EntityId>, caller_instance: EntityId) -> Result<DesktopLeaseOutcome, DesktopError> {
        let session = match self.session(session_id) {
            Ok(s) => s,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        let mut domain = match session.domain_locked() {
            Ok(s) => s,
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        };
        if session.closed.load(Ordering::Relaxed) {
            return Err(DesktopError::rejected("session_closed", "session is closed"));
        }
        let (mut outcome, revoke) = apply_lease_op(&mut domain.lease_state, op, token, caller_instance, session_id)?;
        if revoke {
            // Handover fence: release held inputs and restore remapped rows
            // under the same domain lock, so no new writer can emit input
            // while the old one's unpaired presses are still down.
            outcome.held_released = Self::release_provider_state(&session, &mut domain);
        }
        Ok(outcome)
    }

    // ------------------------------------------------------------------
    // Input injection (observation-bound, lease-gated, never replayed)
    // ------------------------------------------------------------------

    /// Provider-side half of observation verification. The product layer
    /// already read the sealed observation from the CAS and checked its
    /// identity and freshness window; here the live domain state must still
    /// match the observation's capture-time facts.
    fn verify_observation_state(
        session: &X11Session,
        observation: &DesktopObservation,
        require_pixel_fresh: bool,
    ) -> Result<(), DesktopError> {
        if observation.session_id != session.session_id {
            return Err(DesktopError::rejected("observation_session_mismatch", ""));
        }
        if observation.domain_id != session.info.domain_id {
            return Err(DesktopError::rejected("observation_domain_mismatch", ""));
        }
        // A takeover/release bumps the domain epoch; an observation captured
        // under an older epoch was made against coordinates the holder no
        // longer controls and must be re-taken.
        match session.domain_locked() {
            Ok(domain) => {
                if domain.lease_state.epoch != observation.lease_epoch_at_capture {
                    return Err(DesktopError::rejected("observation_stale_epoch", ""));
                }
            }
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        }
        let (fingerprint, _) = Self::layout_fingerprint(&session.conn, session.screen.root)
            .map_err(|_| DesktopError::unavailable("layout_query_failed", ""))?;
        if fingerprint != observation.layout_fingerprint {
            return Err(DesktopError::rejected("observation_stale_layout", ""));
        }
        if require_pixel_fresh {
            let frame = Self::capture_region(session, observation.region)
                .map_err(|reason| DesktopError::unavailable(reason, ""))?;
            if hex_digest(&frame.data) != observation.pixel_hash {
                return Err(DesktopError::rejected("observation_stale_pixels", ""));
            }
        }
        Ok(())
    }

    fn inject_session(&self, session_id: EntityId, mut request: DesktopInjectRequest, operation_id: EntityId) -> Result<DesktopInjectOutcome, DesktopError> {
        let session = match self.session(session_id) {
            Ok(s) => s,
            Err(reason) => return Err(DesktopError::unavailable(reason, "")),
        };
        // Lease check on the shared domain: the token must match the current
        // holder, and it is re-verified before EVERY action — the domain lock
        // is released between actions so an external-input watcher or an
        // explicit takeover can revoke the epoch mid-batch and stop the rest.
        let bound_epoch = match session.domain_locked() {
            Ok(domain) => match &domain.lease_state.holder {
                Some(lease) if lease.token == request.lease_token => domain.lease_state.epoch,
                _ => {
                    let reason = if domain.lease_state.holder.is_some() {
                        "lease_revoked"
                    } else {
                        "no_lease"
                    };
                    return Err(DesktopError::rejected(reason, format!("epoch {}", domain.lease_state.epoch)));
                }
            },
            Err(_) => return Err(DesktopError::unavailable("backend_state_poisoned", "")),
        };
        Self::verify_observation_state(&session, &request.observation, request.require_pixel_fresh)
            .map_err(|error| match error {
                DesktopError::Rejected { reason, detail } => DesktopError::Rejected {
                    reason,
                    detail: format!("epoch {bound_epoch} {detail}"),
                },
                other => other,
            })?;
        if let Ok(mut shared) = session.locked() {
            shared.send_log.insert(operation_id, 0);
        }
        let mut receipts: Vec<DesktopActionReceipt> = vec![];
        let mut sent = 0usize;
        let mut unverifiable = 0usize;
        let mut lost = false;
        let mut cancelled = false;
        let mut failure: Option<&'static str> = None;
        for (index, action) in request.actions.iter().enumerate() {
            // Each action re-validates the lease under the domain lock —
            // per-pointer-event, per-button-action and per-KEYSYM inside
            // Text — so a takeover or external input revokes between any
            // two atomic send units, never only between actions.
            if (request.checkpoint)().is_err() {
                // Durable cancel intent committed by the caller (docs/specs/durable-state.md#spec-10-6):
                // stop the batch, release nothing here — the lease guard in
                // the in-flight action still fences against a takeover.
                cancelled = true;
                failure = Some("cancelled");
                break;
            }
            let result = send_action(&session, request.lease_token, bound_epoch, action, &mut *request.checkpoint);
            match result {
                Ok(receipt) => {
                    sent += receipt.events_sent;
                    if !receipt.postcondition_verified {
                        unverifiable += 1;
                    }
                    receipts.push(receipt.receipt(index));
                }
                Err((SendError::Rejected(reason), partial)) => {
                    // Events ACKed inside this action still count.
                    sent += partial;
                    receipts.push(DesktopActionReceipt {
                        index,
                        kind: action.kind().into(),
                        transport: "rejected",
                        observed_effect: Some(reason.into()),
                    });
                    failure = Some(reason);
                    break;
                }
                Err((SendError::Lost, partial)) => {
                    sent += partial;
                    lost = true;
                    receipts.push(DesktopActionReceipt {
                        index,
                        kind: action.kind().into(),
                        transport: "lost",
                        observed_effect: None,
                    });
                    break;
                }
            }
            if let Ok(mut shared) = session.locked() {
                shared.send_log.insert(operation_id, sent);
            }
        }
        let pointer_post = xproto::query_pointer(&session.conn, session.screen.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|p| [p.root_x, p.root_y]);
        if cancelled {
            // docs/specs/desktop-runtime.md#spec-24-3: on cancellation the plugin releases every key and
            // button it still holds so no modifier stays pressed.
            if let Ok(mut domain) = session.domain_locked() {
                Self::release_provider_state(&session, &mut domain);
            }
        }
        if let Ok(mut shared) = session.locked() {
            shared.send_log.insert(operation_id, sent);
        }
        Ok(DesktopInjectOutcome {
            receipts,
            sent_events: sent,
            postcondition_verified: unverifiable == 0 && sent > 0,
            stopped_reason: if lost {
                Some("transport_lost".to_string())
            } else {
                failure.map(str::to_string)
            },
            detail: serde_json::json!({
                "lease_epoch": bound_epoch,
                "pointer_post": pointer_post,
                "sent_events": sent,
                "cancelled": cancelled,
            }),
        })
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
                        alive = !session.closed.load(Ordering::Relaxed);
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

    fn reconcile_session(&self, session_id: EntityId, operation_id: EntityId) -> DesktopReconcileOutcome {
        let session = self.session(session_id).ok();
        let (outcome, effect, note) = self.reconcile_input(operation_id);
        let _ = session;
        DesktopReconcileOutcome {
            sent_events: note["sent_events_before_recovery"].as_u64().map(|v| v as usize),
            session_alive: note["session_alive_in_memory"].as_bool().unwrap_or(false),
            process_stop_proven: false,
            detail: serde_json::json!({
                "outcome": outcome.map(|o| format!("{o:?}")),
                "effect_status": format!("{effect:?}"),
                "note": note,
            }),
        }
    }
}

impl Drop for X11Provider {
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

/// A send failure carries the events that were already ACKed inside this
/// action so partial effects are never under-counted.
type SendFail = (SendError, usize);

/// One abortable send boundary inside an already-acquired domain guard:
/// emit the XTEST event, then apply the held-input mutation. Callers keep
/// the guard for a whole multi-event sequence so an interleaved revoke
/// cannot slip between an action's own press and release bookkeeping.
fn bounded_input(
    session: &X11Session,
    domain: &mut MutexGuard<'_, DomainState>,
    kind: u8,
    detail: u8,
    x: i16,
    y: i16,
    new_hold: bool,
) -> Result<(), SendError> {
    let sent_ok = xtest::fake_input(&session.conn, kind, detail, 0, session.screen.root, x, y, 0)
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if !sent_ok {
        return Err(SendError::Lost);
    }
    // Track unpaired presses — but ONLY holds this action originated
    // (`new_hold`). A repeat press over a foreign (caller/human) hold is
    // never adopted: cleanup must never release a hold we didn't create.
    match kind {
        KEY_PRESS_EVENT | BUTTON_PRESS_EVENT => {
            if new_hold {
                let input = if kind == KEY_PRESS_EVENT {
                    HeldInput::Key(detail)
                } else {
                    HeldInput::Button(detail)
                };
                domain.held.insert(input);
            }
        }
        KEY_RELEASE_EVENT | BUTTON_RELEASE_EVENT => {
            // Releases always prune our tracked hold — the caller asked to
            // release, so its hold is gone regardless of provenance.
            let input = if kind == KEY_RELEASE_EVENT {
                HeldInput::Key(detail)
            } else {
                HeldInput::Button(detail)
            };
            domain.held.remove(&input);
        }
        _ => {}
    }
    Ok(())
}

/// Read the raw down bitmap of the XTEST slave that receives our events.
/// XI1 QueryDeviceState returns that device's key->down/button->down, not
/// the master aggregate. XIQueryPointer cannot query an attached slave.
/// Button bits use the button number directly (bit 0 is unused); key bits
/// use the keycode. A missing device/class is unknown, never assumed up.
fn xtest_down_bits(session: &X11Session, keyboard: bool) -> Option<[u8; 32]> {
    let conn = &session.conn;
    // Trigger PickPointer on this connection before asking XI which master
    // the server chose. XTEST uses the paired master for keyboard events.
    xproto::query_pointer(conn, session.screen.root).ok()?.reply().ok()?;
    let client_pointer = xinput::xi_get_client_pointer(conn, 0).ok()?.reply().ok()?;
    if !client_pointer.set {
        return None;
    }
    let devices = xinput::xi_query_device(conn, XI_ALL_DEVICES).ok()?.reply().ok()?;
    let master_pointer = client_pointer.deviceid;
    let master_keyboard = devices.infos.iter().find(|device| {
        device.deviceid == master_pointer && device.type_ == xinput::DeviceType::MASTER_POINTER
    })?.attachment;
    let master = if keyboard { master_keyboard } else { master_pointer };
    let wanted = if keyboard {
        xinput::DeviceType::SLAVE_KEYBOARD
    } else {
        xinput::DeviceType::SLAVE_POINTER
    };
    let mut matches = devices.infos.iter().filter(|device| {
        device.enabled
            && device.type_ == wanted
            && device.attachment == master
            && String::from_utf8_lossy(&device.name).contains("XTEST")
    });
    let slave = matches.next()?.deviceid;
    // XI1 device identifiers are limited to 0..127 even though XI2 uses
    // u16 ids. Never query a truncated or ambiguous synthetic device.
    if matches.next().is_some() || slave > 127 {
        return None;
    }
    let slave = u8::try_from(slave).ok()?;
    let reply = xinput::query_device_state(conn, slave).ok()?.reply().ok()?;
    reply.classes.into_iter().find_map(|class| match class.data {
        xinput::InputStateData::Key(state) if keyboard => Some(state.keys),
        xinput::InputStateData::Button(state) if !keyboard => Some(state.buttons),
        _ => None,
    })
}

/// Holds we already own remain ours; a foreign hold on this SAME XTEST
/// slave stays foreign. Physical slaves do not affect this decision: a
/// physically-held button must not hide the new XTEST hold we must pair
/// with a release (or remember for revocation cleanup).
fn input_is_down(domain: &DomainState, input: HeldInput, bits: &[u8; 32]) -> bool {
    let detail = match input {
        HeldInput::Key(code) => code,
        HeldInput::Button(button) => button,
    };
    domain.held.contains(&input)
        || bits[usize::from(detail) / 8] & (1u8 << (detail % 8)) != 0
}

fn send_action(
    session: &X11Session,
    token: EntityId,
    epoch: u64,
    action: &DesktopAction,
    checkpoint: &mut dyn FnMut() -> Result<(), DesktopError>,
) -> Result<SendResult, SendFail> {
    // Cancellation is checked before the action's atomic send unit; a
    // durable cancel intent stops input without waiting for the lease
    // to change.
    checkpoint().map_err(|_| (SendError::Rejected("cancelled"), 0))?;
    match action {
        DesktopAction::PointerMove { x, y } => {
            let x = (*x).clamp(0, i32::from(i16::MAX)) as i16;
            let y = (*y).clamp(0, i32::from(i16::MAX)) as i16;
            {
                // Bound, send, and verify the postcondition under one guard
                // so a takeover cannot interleave between move and query.
                let _domain =
                    session.lease_guard(token, epoch).map_err(|e| (e, 0))?;
                xtest::fake_input(
                    &session.conn, MOTION_NOTIFY_EVENT, 0, 0, session.screen.root, x, y, 0,
                )
                .ok()
                .and_then(|c| c.check().ok())
                .ok_or((SendError::Lost, 0))?;
                // Provable postcondition: real pointer position after ACK.
                let post = xproto::query_pointer(&session.conn, session.screen.root)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .ok_or((SendError::Lost, 1))?;
                Ok(SendResult {
                    events_sent: 1,
                    postcondition_verified: post.root_x == x && post.root_y == y,
                    kind: "pointer_move".into(),
                    detail: Some(format!("pointer={}x{}", post.root_x, post.root_y)),
                })
            }
        }
        DesktopAction::Button { button, press } => {
            if !(1..=7).contains(button) {
                return Err((SendError::Rejected("button_out_of_range"), 0));
            }
            // One guard per button action so the down-state probe and the
            // press/release pair cannot be split by a revoke.
            let mut domain = session.lease_guard(token, epoch).map_err(|e| (e, 0))?;
            match press {
                DesktopPress::Tap => {
                    // The probe and press must not race another XTEST
                    // client's press on this same shared injection slave.
                    let _grab = ServerGrab::acquire(&session.conn).map_err(|e| (e, 0))?;
                    let held = HeldInput::Button(*button);
                    let bits = xtest_down_bits(session, false).ok_or((SendError::Lost, 0))?;
                    let was_down = input_is_down(&domain, held, &bits);
                    bounded_input(session, &mut domain, BUTTON_PRESS_EVENT, *button, 0, 0, !was_down)
                        .map_err(|e| (e, 0))?;
                    let mut sent = 1usize;
                    if !was_down {
                        // Pair this new XTEST-slave press even if a physical
                        // slave is holding the same button independently.
                        bounded_input(session, &mut domain, BUTTON_RELEASE_EVENT, *button, 0, 0, false)
                            .map_err(|e| (e, sent))?;
                        sent += 1;
                    }
                    Ok(SendResult {
                        events_sent: sent,
                        postcondition_verified: false,
                        kind: "button".into(),
                        detail: None,
                    })
                }
                DesktopPress::Down => {
                    // Adopt the hold for cleanup ONLY if it wasn't already
                    // down on this XTEST slave (foreign holds stay foreign).
                    let _grab = ServerGrab::acquire(&session.conn).map_err(|e| (e, 0))?;
                    let bits = xtest_down_bits(session, false).ok_or((SendError::Lost, 0))?;
                    let was_down = input_is_down(&domain, HeldInput::Button(*button), &bits);
                    bounded_input(session, &mut domain, BUTTON_PRESS_EVENT, *button, 0, 0, !was_down)
                        .map_err(|e| (e, 0))?;
                    Ok(SendResult {
                        events_sent: 1,
                        postcondition_verified: false,
                        kind: "button_down".into(),
                        detail: None,
                    })
                }
                DesktopPress::Up => {
                    bounded_input(session, &mut domain, BUTTON_RELEASE_EVENT, *button, 0, 0, false)
                        .map_err(|e| (e, 0))?;
                    Ok(SendResult {
                        events_sent: 1,
                        postcondition_verified: false,
                        kind: "button_up".into(),
                        detail: None,
                    })
                }
            }
        }
        DesktopAction::Key { key, press } => {
            let keysym =
                parse_key_name(key).ok_or((SendError::Rejected("key_name_unknown"), 0))?;
            let press = press.unwrap_or(DesktopPress::Tap);
            send_keysym(session, token, epoch, keysym, press, "key", checkpoint)
        }
        DesktopAction::Text { text } => {
            // Per-CHARACTER boundary: a mid-text takeover stops the rest of
            // the string, and a partial failure keeps the already-sent count.
            let mut sent = 0usize;
            for ch in text.chars() {
                checkpoint().map_err(|_| (SendError::Rejected("cancelled"), sent))?;
                let Some(keysym) = char_to_keysym(ch) else {
                    return Err((SendError::Rejected("text_char_unmappable"), sent));
                };
                match send_keysym(session, token, epoch, keysym, DesktopPress::Tap, "text", checkpoint) {
                    Ok(result) => sent += result.events_sent,
                    Err((e, partial)) => return Err((e, sent + partial)),
                }
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
/// symbol is unmapped, as xdotool does) then send through XTEST with a lease
/// boundary before every event.
fn send_keysym(
    session: &X11Session,
    token: EntityId,
    epoch: u64,
    keysym: Keysym,
    press: DesktopPress,
    kind: &str,
    checkpoint: &mut dyn FnMut() -> Result<(), DesktopError>,
) -> Result<SendResult, SendFail> {
    checkpoint().map_err(|_| (SendError::Rejected("cancelled"), 0))?;
    // One lease-guarded domain section per keysym: keymap resolution (which
    // may mutate the server-global map), the physical down-state probes,
    // and the whole event sequence stay atomic against revocation — the
    // per-event boundary is therefore the keysym, and an interleaved
    // takeover can never slip between a press and its matching release.
    let mut domain = session.lease_guard(token, epoch).map_err(|e| (e, 0))?;
    let _grab = ServerGrab::acquire(&session.conn).map_err(|e| (e, 0))?;
    let mapped = keysym_to_keycode(session, &mut domain, keysym).map_err(|e| (e, 0))?;
    // One slave-state query covers both the key and temporary Shift. A
    // physical hold on another slave cannot hide a new synthetic hold;
    // a foreign client's existing XTEST hold is preserved instead.
    let bits = xtest_down_bits(session, true).ok_or((SendError::Lost, 0))?;
    let key_down = input_is_down(&domain, HeldInput::Key(mapped.code), &bits);
    let shift_free = mapped.shift_code
        .is_none_or(|code| !input_is_down(&domain, HeldInput::Key(code), &bits));
    // Temporary Shift events exist only when the modifier is free for us:
    // an already-down Shift still produces the shifted keysym, and we must
    // neither press-track it nor release the original hold.
    let temporary_shift = if shift_free { mapped.shift_code } else { None };
    let mut sent = 0usize;
    macro_rules! emit {
        ($kind:expr, $detail:expr, $claim:expr) => {
            bounded_input(session, &mut domain, $kind, $detail, 0, 0, $claim)
                .map_err(|e| (e, sent))?;
            sent += 1;
        };
    }
    if let Some(shift_code) = temporary_shift {
        emit!(KEY_PRESS_EVENT, shift_code, true); // only reached when Shift was free — ours to clean up
    }
    match press {
        DesktopPress::Tap => {
            emit!(KEY_PRESS_EVENT, mapped.code, !key_down);
            if !key_down {
                emit!(KEY_RELEASE_EVENT, mapped.code, false);
            }
        }
        DesktopPress::Down => {
            emit!(KEY_PRESS_EVENT, mapped.code, !key_down);
        }
        DesktopPress::Up => {
            emit!(KEY_RELEASE_EVENT, mapped.code, false);
        }
    }
    if let Some(shift_code) = temporary_shift {
        emit!(KEY_RELEASE_EVENT, shift_code, false);
    }
    Ok(SendResult {
        events_sent: sent,
        postcondition_verified: false,
        kind: kind.into(),
        detail: None,
    })
}

/// Bound a keyboard-map compare/write/readback or XTEST slave-state probe
/// and input unit to one X server transaction. Other clients cannot change
/// the compared row or synthetic hold between our check and our effect.
struct ServerGrab<'a>(&'a RustConnection);

impl<'a> ServerGrab<'a> {
    fn acquire(conn: &'a RustConnection) -> Result<Self, SendError> {
        xproto::grab_server(conn)
            .ok()
            .and_then(|cookie| cookie.check().ok())
            .ok_or(SendError::Lost)?;
        Ok(Self(conn))
    }
}

impl Drop for ServerGrab<'_> {
    fn drop(&mut self) {
        let _ = xproto::ungrab_server(self.0)
            .ok()
            .and_then(|cookie| cookie.check().ok());
    }
}

fn keysym_to_keycode(
    session: &X11Session,
    shared: &mut DomainState,
    keysym: Keysym,
) -> Result<MappedKey, SendError> {
    let conn = &session.conn;
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    // Always query the LIVE mapping — no cache. A foreign client can
    // ChangeKeyboardMapping at any moment and its MappingNotify may still
    // be queued behind the domain lock, so the only synchronous guarantee
    // that this resolve sees the latest mapping is to read it now.
    let count = max.saturating_sub(min).saturating_add(1);
    let reply = xproto::get_keyboard_mapping(conn, min, count)
        .ok()
        .and_then(|c| c.reply().ok())
        .ok_or(SendError::Lost)?;
    let (per, keymap) = (usize::from(reply.keysyms_per_keycode), reply.keysyms);
    if per == 0 {
        return Err(SendError::Rejected("key_unmappable"));
    }
    // The real Shift keycode comes from the live modifier map (row 0),
    // never a guessed constant. Failed queries are transport loss, not an
    // empty modifier map.
    let modifiers = xproto::get_modifier_mapping(conn)
        .ok()
        .and_then(|c| c.reply().ok())
        .ok_or(SendError::Lost)?;
    let per_mod = modifiers.keycodes.len() / 8;
    let shift_code = modifiers.keycodes[..per_mod]
        .iter()
        .copied()
        .find(|code| *code != 0);
    for (index, row) in keymap.chunks(per).enumerate() {
        let code = min.saturating_add(index as u8);
        if code > max {
            break;
        }
        if row.first() == Some(&keysym) {
            return Ok(MappedKey {
                code,
                shift_code: None,
            });
        }
        if row.get(1) == Some(&keysym) {
            let Some(code_mod) = shift_code else {
                return Err(SendError::Rejected("shift_modifier_unmapped"));
            };
            return Ok(MappedKey {
                code,
                shift_code: Some(code_mod),
            });
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
            // Re-check this spare row under the server grab: another client
            // can modify it between the initial live mapping and this write.
            let live = xproto::get_keyboard_mapping(conn, code, 1)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .ok_or(SendError::Lost)?;
            if usize::from(live.keysyms_per_keycode) != per
                || live.keysyms.len() != per
                || live.keysyms.iter().any(|symbol| *symbol != 0)
            {
                continue;
            }
            let original = live.keysyms;
            let mut new_row = original.clone();
            new_row[0] = keysym;
            xproto::change_keyboard_mapping(conn, 1, code, per as u8, &new_row)
                .ok()
                .and_then(|c| c.check().ok())
                .ok_or(SendError::Lost)?;
            // Read back the row the server ACTUALLY applied — XKB may
            // canonicalize what we sent (e.g. replicating slot 0 into
            // trailing slots on wide maps), so `new_row` is not a safe
            // ownership baseline. The applied row is.
            let applied = xproto::get_keyboard_mapping(conn, code, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.keysyms)
                .ok_or(SendError::Lost)?;
            if applied.first() != Some(&keysym) {
                // Record the applied row before bailing so restore can undo
                // a mapping the server ACKed but reported differently.
                shared.remapped.push((code, original.clone(), applied));
                return Err(SendError::Lost);
            }
            shared.remapped.push((code, original, applied));
            // A remapped row binds the keysym unshifted at slot 0.
            return Ok(MappedKey {
                code,
                shift_code: None,
            });
        }
    }
    Err(SendError::Rejected("key_unmappable"))
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

/// Restore remapped keycodes — but only rows we still own. Before writing
/// `original` back, re-read the live row: if a foreign client remapped our
/// code after us (live != what we wrote), our original is stale and
/// restoring it would clobber their change — skip it instead.
fn restore_keymap(conn: &RustConnection, remapped: &[(u8, Vec<Keysym>, Vec<Keysym>)]) {
    if remapped.is_empty() {
        return;
    }
    let Ok(_grab) = ServerGrab::acquire(conn) else {
        return;
    };
    for (keycode, original, ours) in remapped {
        let live = xproto::get_keyboard_mapping(conn, *keycode, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.keysyms);
        if live.as_deref() != Some(ours.as_slice()) {
            continue; // row no longer ours — leave the foreign mapping alone
        }
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
                // Keyboard/modifier maps are always queried live at resolve
                // time, so MappingNotify needs no consumer here — the only
                // synchronous freshness guarantee is a live query anyway.
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
                            let notified = if let Ok(mut locked) = strong.lock() {
                                locked.lease_state.holder = None;
                                locked.lease_state.epoch += 1;
                                let held: Vec<HeldInput> = std::mem::take(&mut locked.held).into_iter().collect();
                                let remapped = std::mem::take(&mut locked.remapped);
                                // Handover fence: all old unpaired presses
                                // and mappings are released/restored before
                                // another lease can acquire this domain.
                                release_held(&conn, root, &held);
                                restore_keymap(&conn, &remapped);
                                Some((locked.domain_id.clone(), Arc::clone(&locked.fanout)))
                            } else {
                                None
                            };
                            if let Some((domain_id, fanout)) = notified {
                                fanout.dispatch(&domain_id);
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

/// Bind the domain to the peer of the actual connected socket, not the
/// display spelling or a pre-connect DNS result. Duplicating the fd lets the
/// safe standard-library socket APIs inspect it without taking ownership of
/// or altering the live X11 connection.
fn socket_endpoint_key(conn: &RustConnection) -> std::io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let stream = std::net::TcpStream::from(conn.stream().as_fd().try_clone_to_owned()?);
        match stream.peer_addr() {
            Ok(peer) => return Ok(format!("tcp:{peer}")),
            Err(tcp_error) => {
                let unix = std::os::unix::net::UnixStream::from(
                    conn.stream().as_fd().try_clone_to_owned()?,
                );
                let peer = unix.peer_addr().map_err(|_| tcp_error)?;
                if let Some(path) = peer.as_pathname() {
                    use std::os::unix::ffi::OsStrExt;
                    let mut key = String::from("unix:path:");
                    use std::fmt::Write as _;
                    for byte in path.as_os_str().as_bytes() {
                        let _ = write!(key, "{byte:02x}");
                    }
                    return Ok(key);
                }
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    #[cfg(target_os = "android")]
                    use std::os::android::net::SocketAddrExt;
                    #[cfg(target_os = "linux")]
                    use std::os::linux::net::SocketAddrExt;
                    if let Some(name) = peer.as_abstract_name() {
                        let mut key = String::from("unix:abstract:");
                        use std::fmt::Write as _;
                        for byte in name {
                            let _ = write!(key, "{byte:02x}");
                        }
                        return Ok(key);
                    }
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "X11 Unix peer has no address",
                ))
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsSocket;
        let stream = std::net::TcpStream::from(conn.stream().as_socket().try_clone_to_owned()?);
        Ok(format!("tcp:{}", stream.peer_addr()?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "peer endpoint inspection unsupported on this platform",
        ))
    }
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

/// XTEST source ids used for self-injection suppression in the raw watcher.
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

/// Evidence media type for the provider's own raw pixel bytes. The sealed
/// pixels stay provider-native, so their label must come from the frame's
/// declared byte layout — an X11 ZPixmap buffer is never relabelled as a
/// Win32/macOS BGRA buffer, and an unknown layout is not guessed.
fn raw_pixel_media_type(pixel_format: &str) -> &'static str {
    match pixel_format {
        "x11_zpixmap" => "application/x-x11-zpixmap",
        "bgra8" => "application/x-bgra8",
        "rgba8" => "application/x-rgba8",
        _ => "application/octet-stream",
    }
}

fn computer_use_plugin_id() -> EntityId {
    "5f8b2a1d-9e3c-4a7b-8c1d-2e4f5a6b7c8d"
        .parse()
        .expect("fixed computer-use plugin UUID")
}

// ---------------------------------------------------------------------------
// The X11 provider behind the neutral contract
// ---------------------------------------------------------------------------

impl DesktopSession for X11Session {
    fn session_id(&self) -> EntityId {
        self.session_id
    }

    fn provider_id(&self) -> &'static str {
        "x11"
    }

    fn domain_id(&self) -> String {
        self.info.domain_id.clone()
    }

    fn display(&self) -> DesktopDisplay {
        let info = &self.info;
        let capabilities = vec![
            DesktopCapabilityFact {
                capability: DesktopCapability::ScreenRead,
                available: true,
                detail: "X11 GetImage ZPixmap".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::Input,
                available: info.xtest_version.is_some(),
                detail: format!(
                    "XTEST {}",
                    info.xtest_version.clone().unwrap_or_else(|| "unavailable".into())
                ),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::ExternalInputDetection,
                available: info.external_takeover_detection,
                detail: "XInput2 raw events from non-XTEST source devices".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::LayoutRevision,
                available: true,
                detail: "window tree + live root geometry fingerprint".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::PixelFreshness,
                available: true,
                detail: "GetImage re-capture pixel hash".into(),
            },
        ];
        DesktopDisplay {
            provider: "x11".into(),
            domain_id: info.domain_id.clone(),
            display_id: info.display.clone(),
            session_kind: "x11_display".into(),
            width_px: info.width_px,
            height_px: info.height_px,
            dpi_x: info.dpi_x,
            dpi_y: info.dpi_y,
            scale_factor: 1.0,
            depth: info.depth,
            capabilities,
            detail: serde_json::json!({
                "backend": info.backend,
                "display": info.display,
                "screen_num": info.screen_num,
                "root_window": info.root_window,
                "width_px": info.width_px,
                "height_px": info.height_px,
                "width_mm": info.width_mm,
                "height_mm": info.height_mm,
                "dpi_x": info.dpi_x,
                "dpi_y": info.dpi_y,
                "depth": info.depth,
                "xtest_version": info.xtest_version,
                "xinput2_version": info.xinput2_version,
                "xtest_source_device_ids": info.xtest_source_device_ids,
                "domain_id": info.domain_id,
                "external_takeover_detection": info.external_takeover_detection,
            }),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl DesktopProvider for X11Provider {
    fn provider_id(&self) -> &'static str {
        "x11"
    }

    fn capabilities(&self) -> Vec<DesktopCapabilityFact> {
        // Static declaration: live probing happens per session open and is
        // reported in the session's own capability facts.
        vec![
            DesktopCapabilityFact {
                capability: DesktopCapability::ScreenRead,
                available: true,
                detail: "X11 GetImage".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::Input,
                available: true,
                detail: "XTEST FakeInput".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::ExternalInputDetection,
                available: true,
                detail: "XInput2 raw events; requires XI >= 2.1, negotiated per session".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::LayoutRevision,
                available: true,
                detail: "window tree fingerprint".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::PixelFreshness,
                available: true,
                detail: "re-capture pixel hash".into(),
            },
        ]
    }

    fn open(
        &self,
        request: &DesktopOpenRequest,
        owner_instance: EntityId,
    ) -> Result<DesktopSessionHandle, DesktopError> {
        self.open_provider_session(request, owner_instance)
    }

    fn observe(
        &self,
        handle: &DesktopSessionHandle,
        region: Option<[i32; 4]>,
    ) -> Result<DesktopFrame, DesktopError> {
        self.observe_session(handle.session_id, region)
    }

    fn lease_op(
        &self,
        handle: &DesktopSessionHandle,
        op: DesktopLeaseOp,
        token: Option<EntityId>,
        caller_instance: EntityId,
    ) -> Result<DesktopLeaseOutcome, DesktopError> {
        self.lease_op_session(handle.session_id, op, token, caller_instance)
    }

    fn inject(
        &self,
        handle: &DesktopSessionHandle,
        request: DesktopInjectRequest,
        operation_id: EntityId,
    ) -> Result<DesktopInjectOutcome, DesktopError> {
        self.inject_session(handle.session_id, request, operation_id)
    }

    fn close(
        &self,
        handle: &DesktopSessionHandle,
        caller_instance: EntityId,
    ) -> Result<DesktopCloseOutcome, DesktopError> {
        self.close_session(handle.session_id, caller_instance)
    }

    fn reconcile(
        &self,
        handle: &DesktopSessionHandle,
        operation_id: EntityId,
    ) -> DesktopReconcileOutcome {
        self.reconcile_session(handle.session_id, operation_id)
    }
}

// ---------------------------------------------------------------------------
// Product-level dispatch: operation outcomes across providers
// ---------------------------------------------------------------------------

impl ComputerUseBackend {
    fn open_session(&self, frozen: &SessionFrozen, owner_instance: EntityId) -> DriveOutcome {
        let provider = match self.provider_for(&frozen.backend_name) {
            Ok(provider) => provider,
            Err(error) => return desktop_error_outcome(&error),
        };
        let request = DesktopOpenRequest {
            display: frozen.display.clone(),
            screen: frozen.screen,
        };
        match provider.open(&request, owner_instance) {
            Ok(handle) => {
                let display = handle.session.display();
                let mut session_json = serde_json::Map::new();
                session_json.insert("session_id".into(), serde_json::json!(handle.session_id.to_string()));
                session_json.insert("provider".into(), serde_json::json!(display.provider));
                session_json.insert("domain_id".into(), serde_json::json!(display.domain_id));
                session_json.insert("display_id".into(), serde_json::json!(display.display_id));
                session_json.insert("session_kind".into(), serde_json::json!(display.session_kind));
                session_json.insert("width_px".into(), serde_json::json!(display.width_px));
                session_json.insert("height_px".into(), serde_json::json!(display.height_px));
                session_json.insert("dpi_x".into(), serde_json::json!(display.dpi_x));
                session_json.insert("dpi_y".into(), serde_json::json!(display.dpi_y));
                session_json.insert("scale_factor".into(), serde_json::json!(display.scale_factor));
                session_json.insert("depth".into(), serde_json::json!(display.depth));
                session_json.insert("capabilities".into(), serde_json::json!(display.capabilities));
                // Provider-specific facts keep their historical top-level
                // names (xtest_version, ...) so existing consumers and
                // receipts stay valid.
                if let Some(detail) = display.detail.as_object() {
                    for (key, value) in detail {
                        session_json.insert(key.clone(), value.clone());
                    }
                }
                let entry = DesktopSessionEntry {
                    provider,
                    handle: DesktopSessionHandle::new(
                        handle.session_id,
                        Arc::clone(&handle.session),
                        handle.owner_instance,
                    ),
                };
                if let Ok(mut sessions) = self.sessions.lock() {
                    sessions.insert(entry.handle.session_id, entry);
                }
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::json!({
                        "result": "opened",
                        "session": serde_json::Value::Object(session_json),
                    }),
                )
            }
            Err(error) => desktop_error_outcome(&error),
        }
    }

    fn close_session(&self, frozen: &SessionFrozen, caller_instance: EntityId) -> DriveOutcome {
        let Some(session_id) = frozen.session_id else {
            return DriveOutcome::rejected("session_id_required", serde_json::json!({}));
        };
        let (provider, handle) = match self.session_entry(session_id) {
            Ok(pair) => pair,
            Err(error) => return desktop_error_outcome(&error),
        };
        match provider.close(&handle, caller_instance) {
            Ok(outcome) => {
                if let Ok(mut sessions) = self.sessions.lock() {
                    sessions.remove(&session_id);
                }
                DriveOutcome::done(
                    EffectStatus::None,
                    ObservationLevel::PostconditionVerified,
                    serde_json::json!({
                        "result": "closed",
                        "session_id": session_id,
                        "held_released": outcome.held_released,
                        "lease_revoked": outcome.lease_revoked,
                    }),
                )
            }
            Err(error) => desktop_error_outcome(&error),
        }
    }

    fn observe(&self, frozen: &ObserveFrozen) -> DriveOutcome {
        let (provider, handle) = match self.session_entry(frozen.session_id) {
            Ok(pair) => pair,
            Err(error) => return desktop_error_outcome(&error),
        };
        let frame = match provider.observe(&handle, frozen.region) {
            Ok(frame) => frame,
            Err(error) => return desktop_error_outcome(&error),
        };
        // Seal the raw pixels, the normalized preview and the observation
        // record itself; the record digest is the durable observation id.
        let raw_ref = match self.seal_media(
            &frame.raw_pixels,
            raw_pixel_media_type(&frame.pixel_format.name),
            "computer-use-pixels",
        ) {
            Ok(reference) => reference,
            Err(_) => return DriveOutcome::unavailable("cas_unavailable", String::new()),
        };
        let png = match encode_png(frame.width, frame.height, &frame.rgb) {
            Ok(png) => png,
            Err(reason) => return DriveOutcome::unavailable(reason, String::new()),
        };
        let png_ref = match self.seal_media(&png, "image/png", "computer-use-preview") {
            Ok(reference) => reference,
            Err(_) => return DriveOutcome::unavailable("cas_unavailable", String::new()),
        };
        let display = handle.session.display();
        let observation = DesktopObservation {
            session_id: handle.session_id,
            provider: display.provider.clone(),
            domain_id: display.domain_id.clone(),
            display: display.display_id.clone(),
            display_id: display.display_id.clone(),
            screen_num: frame.screen_num,
            frame_id: frame.frame_id,
            revision: frame.revision,
            captured_at_ms: chrono::Utc::now().timestamp_millis(),
            lease_epoch_at_capture: frame.lease_epoch,
            region: frame.region,
            viewport: [
                u16::try_from(frame.width).unwrap_or(u16::MAX),
                u16::try_from(frame.height).unwrap_or(u16::MAX),
            ],
            dpi_x: frame.dpi_x,
            dpi_y: frame.dpi_y,
            scale_factor: frame.scale_factor,
            coordinate_transform: DesktopCoordinateTransform {
                scale_x: 1.0 / frame.scale_factor,
                scale_y: 1.0 / frame.scale_factor,
                offset_x: 0,
                offset_y: 0,
            },
            layout_fingerprint: frame.layout_fingerprint.clone(),
            pixel_hash: hex_digest(&frame.raw_pixels),
            pixel_format: frame.pixel_format.clone(),
            raw_pixels_ref: raw_ref,
            png_preview_ref: png_ref,
        };
        let observation_ref = match self.seal(&observation, "computer-use-observation-record") {
            Ok(reference) => reference,
            Err(_) => return DriveOutcome::unavailable("cas_unavailable", String::new()),
        };
        DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::PostconditionVerified,
            serde_json::json!({
                "result": "observed",
                "observation_id": observation_ref.digest.0,
                "observation": observation,
                "detail": frame.detail,
            }),
        )
    }

    fn lease_op(&self, frozen: &LeaseFrozen, caller_instance: EntityId) -> DriveOutcome {
        let (provider, handle) = match self.session_entry(frozen.session_id) {
            Ok(pair) => pair,
            Err(error) => return desktop_error_outcome(&error),
        };
        match provider.lease_op(&handle, frozen.op, frozen.lease_token, caller_instance) {
            Ok(outcome) => DriveOutcome::done(
                EffectStatus::None,
                ObservationLevel::RemoteCompleted,
                serde_json::json!({
                    "result": outcome.result,
                    "lease": LeaseNormalized {
                        lease_token: outcome.lease_token,
                        epoch: outcome.epoch,
                        held_released: outcome.held_released,
                    },
                    "revoked": outcome.revoked,
                }),
            ),
            Err(error) => desktop_error_outcome(&error),
        }
    }

    /// Read the sealed observation and check the provider-neutral identity
    /// and freshness window; the provider still checks the live domain
    /// state (epoch, layout, pixels) before any event is sent.
    fn read_observation(&self, observation_id: &str) -> Result<DesktopObservation, DesktopError> {
        let bytes = self
            .store
            .cas
            .read_object(&Digest(observation_id.to_string()))
            .map_err(|_| DesktopError::rejected("observation_unknown", "not sealed in this CAS"))?;
        serde_json::from_slice(&bytes).map_err(|_| DesktopError::rejected("observation_corrupt", "sealed bytes do not parse"))
    }

    /// Recovery view of one injected batch across every live provider
    /// session: in-memory progress only, never replayed or auto-resolved.
    fn reconcile_injected_batch(
        &self,
        operation_id: EntityId,
    ) -> (Option<OperationOutcome>, EffectStatus, serde_json::Value) {
        let mut sent: Option<usize> = None;
        let mut alive = false;
        let mut providers: Vec<&'static str> = Vec::new();
        if let Ok(sessions) = self.sessions.lock() {
            for entry in sessions.values() {
                let report = entry.provider.reconcile(&entry.handle, operation_id);
                if !providers.contains(&entry.provider.provider_id()) {
                    providers.push(entry.provider.provider_id());
                }
                if let Some(count) = report.sent_events {
                    sent = Some(count);
                }
                alive |= report.session_alive;
            }
        }
        let note = serde_json::json!({
            "observation": "controller recovered without a conclusive transport result",
            "providers_reported": providers,
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

    fn inject(&self, frozen: &InjectFrozen, ctx: &DriveCtx) -> DriveOutcome {
        let (provider, handle) = match self.session_entry(frozen.session_id) {
            Ok(pair) => pair,
            Err(error) => return desktop_error_outcome(&error),
        };
        let observation = match self.read_observation(&frozen.observation_id) {
            Ok(observation) => observation,
            Err(error) => return desktop_error_outcome(&error),
        };
        if observation.session_id != handle.session_id {
            return DriveOutcome::rejected("observation_session_mismatch", serde_json::json!({}));
        }
        let now = chrono::Utc::now().timestamp_millis();
        if now.saturating_sub(observation.captured_at_ms) > MAX_OBSERVATION_AGE_MS {
            return DriveOutcome::rejected("observation_expired", serde_json::json!({}));
        }
        // The durable cancel probe: a committed abort intent on this task
        // stops the batch between atomic send units.
        let store = Arc::clone(&self.store);
        let task_id = ctx.task_id;
        let checkpoint = Box::new(move || {
            match store.get_task(task_id) {
                Ok(Some(task)) if task.abort_request.is_some() => Err(DesktopError::rejected(
                    "cancelled",
                    "durable cancel intent committed on the task",
                )),
                Ok(_) => Ok(()),
                Err(error) => Err(DesktopError::unavailable("store_read_failed", error.to_string())),
            }
        });
        let request = DesktopInjectRequest {
            lease_token: frozen.lease_token,
            actions: frozen.actions.clone(),
            observation,
            require_pixel_fresh: frozen.require_pixel_fresh,
            checkpoint,
        };
        match provider.inject(&handle, request, ctx.operation_id) {
            Ok(outcome) => {
                let cancelled = outcome
                    .stopped_reason
                    .as_deref()
                    .is_some_and(|reason| reason == "cancelled");
                let lost = outcome
                    .stopped_reason
                    .as_deref()
                    .is_some_and(|reason| reason == "transport_lost");
                let mut normalized = serde_json::json!({
                    "actions": outcome.receipts,
                    "sent_events": outcome.sent_events,
                });
                if let Some(detail) = outcome.detail.as_object() {
                    for (key, value) in detail {
                        normalized[key] = value.clone();
                    }
                }
                if lost {
                    normalized["result"] = serde_json::json!("incomplete");
                    normalized["reason"] = serde_json::json!("transport_lost");
                    return DriveOutcome {
                        outcome: Some(OperationOutcome::Interrupted),
                        effect: EffectStatus::Unknown,
                        level: None,
                        decode: DecodeStatus::Uninterpreted,
                        normalized,
                    };
                }
                if let Some(reason) = outcome.stopped_reason {
                    if cancelled {
                        // docs/specs/durable-state.md#spec-10-6: the cancel intent stops new input; the
                        // events already delivered stay a partial fact.
                        normalized["result"] = serde_json::json!("cancelled");
                        normalized["reason"] = serde_json::json!("durable_cancel");
                        return DriveOutcome {
                            outcome: Some(OperationOutcome::Interrupted),
                            effect: if outcome.sent_events > 0 {
                                EffectStatus::Partial
                            } else {
                                EffectStatus::None
                            },
                            level: Some(ObservationLevel::RemoteCompleted),
                            decode: DecodeStatus::Decoded,
                            normalized,
                        };
                    }
                    if outcome.sent_events > 0 {
                        normalized["result"] = serde_json::json!("rejected");
                        normalized["reason"] = serde_json::json!(reason);
                        return DriveOutcome {
                            outcome: Some(OperationOutcome::Failed),
                            effect: EffectStatus::Partial,
                            level: Some(ObservationLevel::RemoteCompleted),
                            decode: DecodeStatus::Decoded,
                            normalized,
                        };
                    }
                    return DriveOutcome::rejected_owned(&reason, "");
                }
                normalized["result"] = serde_json::json!("sent");
                let effect = if outcome.postcondition_verified && outcome.sent_events > 0 {
                    EffectStatus::Complete
                } else if outcome.sent_events > 0 {
                    EffectStatus::Partial
                } else {
                    EffectStatus::None
                };
                DriveOutcome::done(
                    effect,
                    if outcome.postcondition_verified && outcome.sent_events > 0 {
                        ObservationLevel::PostconditionVerified
                    } else {
                        ObservationLevel::RemoteCompleted
                    },
                    normalized,
                )
            }
            Err(error) => desktop_error_outcome(&error),
        }
    }
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

const SESSION_PROMPT: &str = "computer.use.session opens/closes a real desktop session on a compiled-in provider {\"backend\":\"x11\"|\"windows\"|\"macos\"|\"auto\",\"display\":\":N\"|desktop-or-display-name|null,\"screen\":int|null,\"op\":\"open\"|\"close\",\"session_id\":uuid?}. Returns session_id, the provider's own mechanism facts (X11: XTEST/XInput2 versions; Windows: input desktop, DPI; macOS: display, Screen Recording/Accessibility grants), display domain identity and DPI. A backend absent from this build, an unreachable display or an ungranted OS permission fails typed-unavailable naming it; a human desktop is never guessed.";
const OBSERVE_PROMPT: &str = "computer.use.observe captures a real framebuffer region through the session's provider (X11 GetImage ZPixmap, Win32 BitBlt, or CGDisplayCreateImage) {\"session_id\":uuid,\"region\":[x,y,w,h]|null}. Returns observation_id binding stable session/display identity, frame_id, revision, layout fingerprint, pixel hash, provider pixel format, viewport and DPI. Coordinates are only valid while that observation re-verifies.";
const INPUT_PROMPT: &str = "computer.use.input injects through the session's real provider transport (X11 XTEST, Win32 SendInput, or CGEventPost) {\"session_id\":uuid,\"lease_token\":uuid,\"observation_id\":\"blake3:…\",\"actions\":[pointer_move|button|key|text],\"require_pixel_fresh\":bool?}. The observation re-verifies and the lease epoch must be live before every atomic send unit; transport ACK is recorded separately from observed effects, dropped delivery stays unknown, nothing replays.";
const LEASE_PROMPT: &str = "computer.use.lease manages the single write lease of one shared display domain {\"session_id\":uuid,\"op\":\"acquire\"|\"release\"|\"takeover\",\"lease_token\":uuid?}. One domain has one writer; takeover revokes the old epoch so late machine input is rejected and held keys/buttons are released. Read-only observation never needs a lease.";
const CAPABILITIES_PROMPT: &str = "computer.use.capabilities reports the real desktop providers compiled into this build, their declared capability facts, and the original-LayerFS VM path's configuration readiness {\"backend\":\"x11\"|\"windows\"|\"macos\"|\"auto\"|null}. Declared provider facts are probed live per session at open; the VM path reports its pinned SDK/provider/companion prerequisites without starting anything. A provider absent from this build is named unavailable, never silently replaced.";
const VM_PROMPT: &str = "computer.use.vm drives the ORIGINAL LayerFS SDK v1.0.0 pinned at 2aaf2552 inside a qualified Linux VM (BoxLite optional; docs/specs/durable-state.md#spec-10-2-1) {\"op\":\"start\"|\"stop\"|\"state\"|\"commit_receipt\",\"name\":\"vm-name\"}. Never re-implements the Store: every Store/branch/workspace operation runs the upstream SDK in the guest via the companion. start/state return the guest capability facts the companion really probed (docker, /dev/fuse, /proc/mounts) and the bound SDK export surface; unprobed requirements stay unprobed. layerfs_committed is the upstream commit receipt only, never product_durable. A missing SDK path or VM provider is typed-unavailable evidence, not completion.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilitiesInput {
    /// One provider id, or null/absent for every registered provider.
    backend: Option<String>,
}

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
pub struct ComputerUseCapabilitiesHandler {
    backend: Arc<ComputerUseBackend>,
}
pub struct ComputerUseVmHandler {
    backend: Arc<ComputerUseBackend>,
}

impl ComputerUseVmHandler {
    pub fn new(backend: Arc<ComputerUseBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: VmInput = serde_json::from_value(invocation.input.clone())?;
        ensure!(!input.name.is_empty() && input.name.len() <= 128, "vm name must be 1..=128 chars");
        ensure!(
            input.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "vm name must be [a-zA-Z0-9._-]+"
        );
        let frozen = VmFrozen {
            op: input.op,
            name: input.name.clone(),
        };
        self.backend
            .drive(
                invocation,
                "computer.use.vm",
                "computer-use-vm",
                frozen.clone(),
                "computer-use-vm-input",
                serde_json::json!({
                    "mechanism": "original LayerFS SDK via companion process on a qualified Linux VM",
                    "sdk_pin": crate::layerfs::LAYERFS_SDK_PIN,
                }),
                "the named VM state reported by the companion is what the provider actually returned; missing SDK/provider is named typed-unavailable",
                move |backend, _ctx| match frozen.op {
                    VmVerb::Start => {
                        vm_outcome(backend.layerfs.start_vm(&frozen.name))
                    }
                    VmVerb::Stop => vm_outcome(backend.layerfs.stop_vm(&frozen.name)),
                    VmVerb::State => vm_outcome(backend.layerfs.vm_state(&frozen.name)),
                    VmVerb::CommitReceipt => {
                        match backend.layerfs.commit_receipt(&frozen.name) {
                            Ok(receipt) => DriveOutcome::done(
                                EffectStatus::None,
                                ObservationLevel::RemoteCompleted,
                                serde_json::json!({
                                    "result": "committed",
                                    "layerfs_committed": true,
                                    "receipt": receipt,
                                    "note": "layerfs_committed only; product_durable requires this product's own durable publication",
                                }),
                            ),
                            Err(error) => desktop_error_outcome(&error),
                        }
                    }
                },
            )
            .await
    }
}

fn vm_outcome(result: Result<crate::layerfs::LayerFsVmState, DesktopError>) -> DriveOutcome {
    match result {
        Ok(state) => DriveOutcome::done(
            EffectStatus::None,
            ObservationLevel::RemoteCompleted,
            serde_json::json!({
                "result": match state.state.as_str() {
                    "running" => "started",
                    "stopped" => "stopped",
                    other => other,
                },
                "vm": state,
            }),
        ),
        Err(error) => desktop_error_outcome(&error),
    }
}

impl ComputerUseCapabilitiesHandler {
    pub fn new(backend: Arc<ComputerUseBackend>) -> Self {
        Self { backend }
    }

    async fn exec(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: CapabilitiesInput = serde_json::from_value(invocation.input.clone())?;
        if let Some(name) = input.backend.clone() {
            ensure!(name.len() <= 64, "backend name too long");
        }
        let frozen = serde_json::json!({ "backend": input.backend });
        self.backend
            .drive(
                invocation,
                "computer.use.capabilities",
                "computer-use-capabilities",
                frozen.clone(),
                "computer-use-capabilities-input",
                serde_json::json!({"mechanism": "compiled-in provider registry"}),
                "every returned provider id resolves through the registry; unknown providers are reported unavailable by name",
                move |backend, _ctx| {
                    let requested = frozen_backend(&frozen);
                    DriveOutcome::done(
                        EffectStatus::None,
                        ObservationLevel::PostconditionVerified,
                        backend.capabilities_facts(requested.as_deref()),
                    )
                },
            )
            .await
    }
}

fn frozen_backend(frozen: &serde_json::Value) -> Option<String> {
    frozen
        .get("backend")
        .and_then(|value| value.as_str())
        .map(str::to_string)
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
                    backend_name: name.clone(),
                    display: parsed.display.clone(),
                    screen: parsed.screen,
                    session_id: None,
                };
                self.backend
                    .drive(
                        invocation,
                        "computer.use.session",
                        "computer-use-session",
                        frozen.clone(),
                        "computer-use-session-input",
                        serde_json::json!({
                            "backend": name,
                            "mechanism": "compiled-in provider registry resolves the requested backend at open (x11: x11rb connection; windows: user32/gdi32 FFI; macos: CoreGraphics FFI)",
                        }),
                        "the resolved provider establishes a real display session and reports its own probed capability facts, or a typed-unavailable result names the exact missing capability",
                        move |backend, ctx| backend.open_session(&frozen, ctx.caller),
                    )
                    .await
            }
            SessionVerb::Close => {
                let Some(session_id) = parsed.session_id else {
                    return Err(anyhow::anyhow!("close requires session_id"));
                };
                let frozen = SessionFrozen {
                    backend_name: String::new(),
                    display: None,
                    screen: None,
                    session_id: Some(session_id),
                };
                self.backend
                    .drive(
                        invocation,
                        "computer.use.session",
                        "computer-use-session",
                        frozen.clone(),
                        "computer-use-session-input",
                        serde_json::json!({"mechanism": "provider session teardown: display connection/handle dropped, lease epoch revoked"}),
                        "the session's provider resources are dropped and its lease epoch revoked when this session was the writer or the domain's last session",
                        move |backend, ctx| backend.close_session(&frozen, ctx.caller),
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
                serde_json::json!({
                    "mechanism": "provider capture (x11: GetImage ZPixmap; windows: GDI BitBlt; macos: CGDisplayCreateImage) + live layout fingerprint",
                }),
                "raw pixels hash to the recorded pixel_hash in the provider's own byte layout and the recorded layout fingerprint re-computes over the live desktop",
                move |backend, _ctx| backend.observe(&frozen),
            )
            .await
    }
}

#[async_trait]
impl NativeTaskHandler for ComputerUseCapabilitiesHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "computer.use.capabilities",
            "computer_use.capabilities/v1",
            CAPABILITIES_PROMPT,
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
        reconcile_any(&self.backend, task, operation_id, "computer.use.capabilities").await
    }
}

#[async_trait]
impl NativeTaskHandler for ComputerUseVmHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        descriptor_for(
            "computer.use.vm",
            "computer_use.layerfs.vm/v1",
            VM_PROMPT,
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
        reconcile_any(&self.backend, task, operation_id, "computer.use.vm").await
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
            if let DesktopAction::Text { text } = action {
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
                serde_json::json!({
                    "mechanism": "provider input transport (x11: XTEST FakeInput; windows: SendInput; macos: CGEventPost); observation re-verify; lease epoch check",
                }),
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
                move |backend, ctx| backend.lease_op(&frozen, ctx.caller),
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
        // Historical authority spelling kept so operations armed before the
        // provider-neutral cutover keep their recorded authority identity; it
        // is an id, not a platform claim (see SESSION_PROMPT).
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
        let (outcome, effect, note) = self.backend.reconcile_injected_batch(operation_id);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backend() -> Arc<ComputerUseBackend> {
        let dir = std::env::temp_dir().join(format!("shenko-computer-use-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).expect("store"));
        Arc::new(ComputerUseBackend::new(store))
    }

    /// The sealed raw pixels keep their provider's byte layout. A Win32 or
    /// macOS BGRA buffer must never be labelled as an X11 ZPixmap, and an
    /// unknown layout is not guessed into one of them.
    #[test]
    fn raw_pixel_media_type_is_bound_to_the_provider_format() {
        assert_eq!(raw_pixel_media_type("x11_zpixmap"), "application/x-x11-zpixmap");
        assert_eq!(raw_pixel_media_type("bgra8"), "application/x-bgra8");
        assert_eq!(raw_pixel_media_type("rgba8"), "application/x-rgba8");
        assert_eq!(raw_pixel_media_type("planar_y8"), "application/octet-stream");
        for name in ["bgra8", "rgba8", "planar_y8"] {
            assert!(
                !raw_pixel_media_type(name).contains("x11"),
                "{name} must not be labelled as X11 pixels"
            );
        }
    }

    /// Recovery never replays and never names a platform transport: with no
    /// live session the batch stays Unknown, reports no sent-event count and
    /// says so in provider-neutral words.
    #[test]
    fn recovery_of_an_armed_batch_stays_unknown_and_provider_neutral() {
        let backend = test_backend();
        let (outcome, effect, note) = backend.reconcile_injected_batch(EntityId::new());
        assert!(outcome.is_none(), "no conclusive result is invented");
        assert_eq!(effect, EffectStatus::Unknown);
        assert!(note["sent_events_before_recovery"].is_null());
        assert_eq!(note["session_alive_in_memory"], serde_json::json!(false));
        assert_eq!(note["process_stop_proven"], serde_json::json!(false));
        assert_eq!(note["providers_reported"], serde_json::json!([]));
        let text = note.to_string();
        assert!(text.contains("transport"));
        assert!(!text.contains("XTEST"), "recovery text must not claim XTEST");
        assert!(!text.contains("x11"));
    }

    /// Capability discovery reports this build's real providers plus the
    /// original-LayerFS VM path's own prerequisites, and it names an absent
    /// backend instead of silently resolving another one.
    #[test]
    fn capability_discovery_binds_platforms_and_layerfs_prerequisites() {
        let backend = test_backend();
        let all = backend.capabilities_facts(None);
        let providers = all["providers"].as_array().expect("providers array");
        assert!(
            providers
                .iter()
                .any(|entry| entry["provider"] == serde_json::json!("x11")),
            "the X11 provider is compiled into every build"
        );
        #[cfg(not(target_os = "windows"))]
        assert!(
            providers
                .iter()
                .all(|entry| entry["provider"] != serde_json::json!("windows")),
            "a provider absent from this build is not advertised"
        );
        let vm = &all["layerfs_vm"];
        assert_eq!(vm["provider"], serde_json::json!("layerfs_vm"));
        assert_eq!(vm["sdk_pin"], serde_json::json!(crate::layerfs::LAYERFS_SDK_PIN));
        assert_eq!(
            vm["guest_requirements"].as_array().map(|items| items.len()),
            Some(5)
        );
        let state = vm["state"].as_str().expect("vm state");
        assert!(
            [
                "sdk_path_unset",
                "sdk_path_missing",
                "companion_script_missing",
                "configured_vm_not_started",
            ]
            .contains(&state),
            "unexpected layerfs readiness state {state}"
        );

        #[cfg(not(target_os = "windows"))]
        {
            let windows = backend.capabilities_facts(Some("windows"));
            let entry = &windows["providers"][0];
            assert_eq!(entry["available"], serde_json::json!(false));
            assert_eq!(
                entry["reason"],
                serde_json::json!("backend_not_available_on_this_build")
            );
            assert_eq!(entry["provider"], serde_json::json!("windows"));
        }
    }
}
