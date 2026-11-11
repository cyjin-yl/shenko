//! Provider-neutral desktop / VM computer-use contract — docs/specs/desktop-runtime.md#spec-24-1, docs/specs/desktop-runtime.md#spec-24-3,
//! docs/specs/desktop-runtime.md#spec-24-4, docs/specs/desktop-runtime.md#spec-34-4.
//!
//! One display domain has one write lease; observation identity, revision,
//! DPI and coordinate transform are provider-independent facts; a provider
//! only owns what is genuinely platform-specific (how a frame is captured,
//! how an event is injected, how external input is noticed).
//!
//! What this module deliberately does NOT do:
//! - It does not keep a second truth store. Every lease/epoch/held-input
//!   fact lives in the provider's own domain state, and the *algorithm*
//!   that mutates it is the single [`apply_lease_op`] implementation used
//!   by every provider.
//! - It does not bless a platform. A provider that cannot reach a real
//!   display returns a typed error naming the missing capability; it never
//!   fabricates an observation or an effect.
//! - It does not own the product ledger: operations, evidence and effects
//!   stay in the Store contracts used by `computer_use.rs`.
//!
//! The observation record is content-bound: its blake3 digest is the
//! `observation_id` an input batch must carry, and re-verification compares
//! the live layout fingerprint, the live lease epoch and (optionally) the
//! live pixels against that record. A stale coordinate is therefore never
//! silently applied to a new screen.

use serde::{Deserialize, Serialize};
use shenko_types::{ids::EntityId, object::ObjectRef};
use std::sync::Arc;

/// A real sub-capability a desktop provider may declare (docs/specs/desktop-runtime.md#spec-24-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopCapability {
    /// Read the screen pixels of one display.
    ScreenRead,
    /// Inject pointer/keyboard events into the display domain.
    Input,
    /// Notice input that did not originate from this product.
    ExternalInputDetection,
    /// Recompute a layout fingerprint that invalidates stale coordinates.
    LayoutRevision,
    /// Re-capture and compare pixels to prove an observation is current.
    PixelFreshness,
    /// Read accessibility elements of the platform.
    AccessibilityRead,
    /// Read/write the platform clipboard.
    Clipboard,
    /// Transfer files through the platform.
    FileTransfer,
    /// Drive a browser context through the platform.
    BrowserContext,
}

/// A capability with the real evidence that backs it on this node.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopCapabilityFact {
    pub capability: DesktopCapability,
    pub available: bool,
    /// Why it is (not) available — an actual platform fact.
    pub detail: String,
}

/// Typed failure. `Unavailable` names a missing real capability;
/// `Rejected` means the request itself was refused; `Lost` means the
/// transport died and delivery is unprovable.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DesktopError {
    #[error("unavailable: {reason} ({detail})")]
    Unavailable { reason: String, detail: String },
    #[error("rejected: {reason} ({detail})")]
    Rejected { reason: String, detail: String },
    #[error("transport lost: {detail}")]
    Lost { detail: String },
}

impl DesktopError {
    pub fn unavailable(reason: &str, detail: impl Into<String>) -> Self {
        DesktopError::Unavailable {
            reason: reason.to_string(),
            detail: detail.into(),
        }
    }

    pub fn rejected(reason: &str, detail: impl Into<String>) -> Self {
        DesktopError::Rejected {
            reason: reason.to_string(),
            detail: detail.into(),
        }
    }

    pub fn lost(detail: impl Into<String>) -> Self {
        DesktopError::Lost {
            detail: detail.into(),
        }
    }

    /// Stable machine-readable reason, used verbatim in receipts.
    pub fn reason(&self) -> &str {
        match self {
            DesktopError::Unavailable { reason, .. } => reason,
            DesktopError::Rejected { reason, .. } => reason,
            DesktopError::Lost { .. } => "transport_lost",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            DesktopError::Unavailable { detail, .. } => detail,
            DesktopError::Rejected { detail, .. } => detail,
            DesktopError::Lost { detail, .. } => detail,
        }
    }

    /// Whether the failure is environmental (no usable host/display or the
    /// transport dropped) rather than a genuine provider rejection.
    pub fn is_environment(&self) -> bool {
        !matches!(self, DesktopError::Rejected { .. })
    }
}

/// Stable identity and geometry of one display, as the platform reports it.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopDisplay {
    /// Provider id ("x11", "windows", "macos").
    pub provider: String,
    /// The interactive display domain: everything that shares focus, mouse
    /// and keyboard. One domain has one write lease.
    pub domain_id: String,
    /// Provider-stable display identity inside the domain.
    pub display_id: String,
    /// What kind of session this is ("x11_display", "windows_input_desktop",
    /// "macos_console").
    pub session_kind: String,
    pub width_px: u16,
    pub height_px: u16,
    /// Physical DPI as the platform reports it; 0.0 means the platform does
    /// not expose it (never a guess).
    pub dpi_x: f64,
    pub dpi_y: f64,
    /// Backing-store scale (1.0 when physical pixels are logical pixels).
    pub scale_factor: f64,
    pub depth: u8,
    /// Real, probed capabilities of this session.
    pub capabilities: Vec<DesktopCapabilityFact>,
    /// Provider-specific extras (extension versions, device ids, ...).
    pub detail: serde_json::Value,
}

/// The pixel layout of a captured frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopPixelFormat {
    /// Provider-native name of the byte layout ("x11_zpixmap", "bgra8",
    /// "rgba8").
    pub name: String,
    pub depth: u8,
    pub bits_per_pixel: u8,
    pub byte_order: String,
    pub visual_id: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

/// One captured frame. `raw_pixels` are the provider-native bytes that are
/// sealed as evidence; their blake3 digest is the observation's
/// `pixel_hash`. `rgb` is the normalized RGB8 preview.
#[derive(Debug, Clone)]
pub struct DesktopFrame {
    pub region: [i32; 4],
    pub width: u32,
    pub height: u32,
    pub raw_pixels: Vec<u8>,
    pub rgb: Vec<u8>,
    pub pixel_format: DesktopPixelFormat,
    /// Recomputed layout fingerprint; a change invalidates coordinates.
    pub layout_fingerprint: String,
    pub dpi_x: f64,
    pub dpi_y: f64,
    pub scale_factor: f64,
    /// Monotonic per-session frame counter of the provider.
    pub frame_id: u64,
    /// Monotonic per-session observation revision of the provider.
    pub revision: u64,
    /// The display-domain lease epoch that was live at capture time.
    pub lease_epoch: u64,
    /// Provider-native screen index inside the display.
    pub screen_num: usize,
    /// Provider-specific observation extras (window list, ...).
    pub detail: serde_json::Value,
}

/// Physical→logical coordinate transform bound to one observation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DesktopCoordinateTransform {
    pub scale_x: f64,
    pub scale_y: f64,
    pub offset_x: i32,
    pub offset_y: i32,
}

impl Default for DesktopCoordinateTransform {
    fn default() -> Self {
        Self {
            scale_x: 1.0,
            scale_y: 1.0,
            offset_x: 0,
            offset_y: 0,
        }
    }
}

/// The sealed, content-bound observation record. Its blake3 digest is the
/// `observation_id` that later input must carry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopObservation {
    pub session_id: EntityId,
    /// Provider that produced the observation.
    pub provider: String,
    pub domain_id: String,
    /// Provider display identity.
    pub display: String,
    pub display_id: String,
    pub screen_num: usize,
    /// Monotonic per-session frame counter.
    pub frame_id: u64,
    /// Monotonic per-session observation revision. Every published
    /// observation increments it, so a consumer can order observations
    /// without comparing pixels.
    pub revision: u64,
    pub captured_at_ms: i64,
    pub lease_epoch_at_capture: u64,
    /// Captured rectangle in root coordinates [x, y, w, h].
    pub region: [i32; 4],
    pub viewport: [u16; 2],
    pub dpi_x: f64,
    pub dpi_y: f64,
    pub scale_factor: f64,
    pub coordinate_transform: DesktopCoordinateTransform,
    /// Provider layout revision (window tree hash, ...).
    pub layout_fingerprint: String,
    /// blake3 digest of the sealed raw pixels.
    pub pixel_hash: String,
    pub pixel_format: DesktopPixelFormat,
    pub raw_pixels_ref: ObjectRef,
    pub png_preview_ref: ObjectRef,
}

/// One atomic input intent (docs/specs/desktop-runtime.md#spec-24-1 "input").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesktopAction {
    PointerMove { x: i32, y: i32 },
    Button { button: u8, press: DesktopPress },
    Key { key: String, press: Option<DesktopPress> },
    Text { text: String },
}

/// How one key/button action is delivered.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopPress {
    /// One press followed by one release.
    Tap,
    Down,
    Up,
}

impl DesktopAction {
    pub fn kind(&self) -> &'static str {
        match self {
            DesktopAction::PointerMove { .. } => "pointer_move",
            DesktopAction::Button { .. } => "button",
            DesktopAction::Key { .. } => "key",
            DesktopAction::Text { .. } => "text",
        }
    }
}

/// The lease holder of one display domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopLease {
    /// The admitted controller instance holding the epoch.
    pub holder_instance: EntityId,
    /// The session through which the holder writes; losing THIS session
    /// revokes the epoch.
    pub holder_session: EntityId,
    pub token: EntityId,
}

/// The provider-owned lease truth of one display domain: the epoch and the
/// current holder. Providers keep it inside their own domain lock so an
/// epoch bump and the release of machine-held input stay atomic.
#[derive(Debug, Default)]
pub struct DesktopLeaseState {
    /// Monotonic domain epoch; every revocation bumps it and invalidates
    /// every bound token and observation epoch.
    pub epoch: u64,
    pub holder: Option<DesktopLease>,
}

impl DesktopLeaseState {
    pub fn new() -> Self {
        Self {
            epoch: 0,
            holder: None,
        }
    }

    /// True when the token/session/epoch triple still identifies the live
    /// writer of this domain.
    pub fn matches(&self, token: EntityId, holder_session: EntityId, epoch: u64) -> bool {
        self.epoch == epoch
            && self
                .holder
                .is_some_and(|lease| lease.token == token && lease.holder_session == holder_session)
    }
}

/// The lease operations a caller may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopLeaseOp {
    Acquire,
    Release,
    Takeover,
}

impl DesktopLeaseOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            DesktopLeaseOp::Acquire => "acquire",
            DesktopLeaseOp::Release => "release",
            DesktopLeaseOp::Takeover => "takeover",
        }
    }
}

/// The normalized result of one lease operation.
#[derive(Debug, Clone)]
pub struct DesktopLeaseOutcome {
    /// "acquired" | "held" | "released" | "taken_over".
    pub result: &'static str,
    pub lease_token: Option<EntityId>,
    pub epoch: u64,
    /// Number of machine-held inputs released by this operation.
    pub held_released: usize,
    /// True when the operation revoked a previous writer.
    pub revoked: bool,
}

/// The single lease algorithm shared by every provider (docs/specs/desktop-runtime.md#spec-24-3).
///
/// The caller MUST hold the provider's domain lock while calling this, so
/// the epoch bump and the release of machine-held input stay atomic
/// (handover fence): no other writer can emit input between the revoke and
/// the release.
///
/// Returns the normalized outcome and whether the caller must now release
/// provider-held inputs.
pub fn apply_lease_op(
    state: &mut DesktopLeaseState,
    op: DesktopLeaseOp,
    token: Option<EntityId>,
    caller_instance: EntityId,
    holder_session: EntityId,
) -> Result<(DesktopLeaseOutcome, bool), DesktopError> {
    match op {
        DesktopLeaseOp::Acquire => {
            if let Some(lease) = state.holder {
                if lease.holder_instance == caller_instance
                    && lease.holder_session == holder_session
                {
                    return Ok((
                        DesktopLeaseOutcome {
                            result: "held",
                            lease_token: Some(lease.token),
                            epoch: state.epoch,
                            held_released: 0,
                            revoked: false,
                        },
                        false,
                    ));
                }
                return Err(DesktopError::rejected(
                    "lease_conflict",
                    "another controller holds this display domain",
                ));
            }
            let token = EntityId::new();
            state.holder = Some(DesktopLease {
                holder_instance: caller_instance,
                holder_session,
                token,
            });
            Ok((
                DesktopLeaseOutcome {
                    result: "acquired",
                    lease_token: Some(token),
                    epoch: state.epoch,
                    held_released: 0,
                    revoked: false,
                },
                false,
            ))
        }
        DesktopLeaseOp::Release => {
            let owns = state
                .holder
                .as_ref()
                .is_some_and(|lease| Some(lease.token) == token);
            if !owns {
                return Err(DesktopError::rejected(
                    "lease_token_mismatch",
                    "the supplied lease token is not the live writer",
                ));
            }
            state.holder = None;
            state.epoch += 1;
            Ok((
                DesktopLeaseOutcome {
                    result: "released",
                    lease_token: None,
                    epoch: state.epoch,
                    held_released: 0,
                    revoked: true,
                },
                true,
            ))
        }
        DesktopLeaseOp::Takeover => {
            // Explicit takeover always revokes the previous epoch so late
            // machine input from the old controller is rejected, then grants
            // the caller a fresh lease. Never a no-op.
            state.holder = None;
            state.epoch += 1;
            let token = EntityId::new();
            state.holder = Some(DesktopLease {
                holder_instance: caller_instance,
                holder_session,
                token,
            });
            Ok((
                DesktopLeaseOutcome {
                    result: "taken_over",
                    lease_token: Some(token),
                    epoch: state.epoch,
                    held_released: 0,
                    revoked: true,
                },
                true,
            ))
        }
    }
}

/// What a caller asks a provider to open.
#[derive(Debug, Clone, Default)]
pub struct DesktopOpenRequest {
    /// Provider-native display address (X11 ":99", Windows desktop name,
    /// macOS display id). Empty means the platform default.
    pub display: Option<String>,
    /// Provider-native screen index, when the platform has several.
    pub screen: Option<i32>,
}

/// One live desktop session owned by a provider.
pub trait DesktopSession: Send + Sync {
    fn session_id(&self) -> EntityId;
    fn provider_id(&self) -> &'static str;
    /// The interactive display domain this session writes into.
    fn domain_id(&self) -> String;
    fn display(&self) -> DesktopDisplay;
    /// Lock-free liveness: a closed session must answer without waiting on
    /// an input-critical lock.
    fn is_closed(&self) -> bool;
}

/// A provider's live session handle.
pub struct DesktopSessionHandle {
    pub session_id: EntityId,
    pub session: Arc<dyn DesktopSession>,
    /// The admitted controller instance that opened this session.
    pub owner_instance: EntityId,
}

impl DesktopSessionHandle {
    pub fn new(
        session_id: EntityId,
        session: Arc<dyn DesktopSession>,
        owner_instance: EntityId,
    ) -> Self {
        Self {
            session_id,
            session,
            owner_instance,
        }
    }
}

/// One delivered input unit.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopActionReceipt {
    pub index: usize,
    pub kind: String,
    /// "acked" = the platform accepted the event; never an application
    /// effect. "rejected" = refused before leaving the process. "lost" =
    /// delivery unprovable.
    pub transport: &'static str,
    pub observed_effect: Option<String>,
}

/// What a provider reports after an input batch.
#[derive(Debug, Clone)]
pub struct DesktopInjectOutcome {
    pub receipts: Vec<DesktopActionReceipt>,
    pub sent_events: usize,
    /// True when every delivered action had a provable postcondition.
    pub postcondition_verified: bool,
    /// Why the batch stopped, when it did not run to completion
    /// ("cancelled", "lease_revoked", "observation_stale_layout", ...).
    pub stopped_reason: Option<String>,
    /// Provider-specific facts merged into the operation result
    /// (pointer position, ...).
    pub detail: serde_json::Value,
}

/// The input request handed to a provider. The provider re-checks the lease
/// itself before every atomic send unit; `checkpoint` is the product's
/// lock-free cancellation probe (durable cancel intent, session liveness).
pub struct DesktopInjectRequest {
    pub lease_token: EntityId,
    pub actions: Vec<DesktopAction>,
    /// The observation this batch is bound to (already verified by the
    /// product layer for identity and freshness).
    pub observation: DesktopObservation,
    /// Re-capture the observed region and require an exact pixel match
    /// before any event is sent.
    pub require_pixel_fresh: bool,
    /// Called before every atomic send unit; `Err` stops the batch.
    pub checkpoint: Box<dyn FnMut() -> Result<(), DesktopError> + Send>,
}

/// The result of closing a session.
#[derive(Debug, Clone)]
pub struct DesktopCloseOutcome {
    pub held_released: usize,
    pub lease_revoked: bool,
}

/// Recovery facts for an operation whose controller restarted.
#[derive(Debug, Clone)]
pub struct DesktopReconcileOutcome {
    /// Events delivered before the restart, when the provider still knows.
    pub sent_events: Option<usize>,
    /// True when the session is still alive in this process.
    pub session_alive: bool,
    /// True only when the provider proved the remote effect stopped.
    pub process_stop_proven: bool,
    pub detail: serde_json::Value,
}

/// A real desktop/VM provider. Implementations own their platform's
/// sessions and the write-lease truth of the display domains they expose.
pub trait DesktopProvider: Send + Sync {
    /// Stable provider id ("x11", "windows", "macos").
    fn provider_id(&self) -> &'static str;
    /// Real, probed capabilities on this node.
    fn capabilities(&self) -> Vec<DesktopCapabilityFact>;
    /// Open a session on a display domain.
    fn open(
        &self,
        request: &DesktopOpenRequest,
        owner_instance: EntityId,
    ) -> Result<DesktopSessionHandle, DesktopError>;
    /// Capture one frame. Read-only: no lease is required.
    fn observe(
        &self,
        handle: &DesktopSessionHandle,
        region: Option<[i32; 4]>,
    ) -> Result<DesktopFrame, DesktopError>;
    /// One lease operation over the session's display domain.
    fn lease_op(
        &self,
        handle: &DesktopSessionHandle,
        op: DesktopLeaseOp,
        token: Option<EntityId>,
        caller_instance: EntityId,
    ) -> Result<DesktopLeaseOutcome, DesktopError>;
    /// Inject a batch bound to a verified observation. `operation_id` is
    /// the product ledger operation this batch belongs to; the provider
    /// records sent-event counts under it for recovery reporting.
    fn inject(
        &self,
        handle: &DesktopSessionHandle,
        request: DesktopInjectRequest,
        operation_id: EntityId,
    ) -> Result<DesktopInjectOutcome, DesktopError>;
    /// Close the session; releases the lease when this session holds it.
    fn close(
        &self,
        handle: &DesktopSessionHandle,
        caller_instance: EntityId,
    ) -> Result<DesktopCloseOutcome, DesktopError>;
    /// Recovery facts after a controller restart.
    fn reconcile(
        &self,
        handle: &DesktopSessionHandle,
        operation_id: EntityId,
    ) -> DesktopReconcileOutcome;
}

/// blake3 hex digest used for observation/pixel identity.
pub fn hex_digest(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Minimal RGB8 PNG encoder for observation previews.
pub fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, &'static str> {
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

/// The provider ids this build can actually offer, in preference order for
/// the "auto" backend. Each entry is a real implementation in this crate.
pub fn platform_default_provider() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "x11"
    }
}

// ---------------------------------------------------------------------------
// Cross-provider human-input observation (docs/specs/desktop-runtime.md#spec-24-3, docs/specs/browser.md#spec-24-6-2)
// ---------------------------------------------------------------------------

/// Implemented by any provider that must react when a human takes over a
/// shared display domain (the browser drops its agent lease and bumps its
/// controller epoch). The desktop provider calls it from its single
/// external-input detection site with the display domain id; the domain id
/// spelling is owned by the desktop provider and shared by contract.
pub trait HumanInputObserver: Send + Sync {
    fn on_human_input(&self, domain_id: &str);
}

/// The registry the desktop backend dispatches human-input observations
/// through. One truth: every provider reacts to the same domain event.
pub struct HumanInputFanout {
    observers: parking_lot::Mutex<Vec<std::sync::Weak<dyn HumanInputObserver>>>,
}

impl HumanInputFanout {
    pub fn new() -> Self {
        Self {
            observers: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn register(&self, observer: std::sync::Arc<dyn HumanInputObserver>) {
        self.observers.lock().push(std::sync::Arc::downgrade(&observer));
    }

    /// Called at the desktop provider's external-input detection site only.
    pub fn dispatch(&self, domain_id: &str) {
        // The observer's owning plugins retain it. This cross-provider link
        // must not keep a retired browser provider and its sessions alive.
        let observers = {
            let mut registrations = self.observers.lock();
            let mut live = Vec::with_capacity(registrations.len());
            registrations.retain(|observer| {
                if let Some(observer) = observer.upgrade() {
                    live.push(observer);
                    true
                } else {
                    false
                }
            });
            live
        };
        for observer in observers {
            observer.on_human_input(domain_id);
        }
    }
}

impl Default for HumanInputFanout {
    fn default() -> Self {
        Self::new()
    }
}
