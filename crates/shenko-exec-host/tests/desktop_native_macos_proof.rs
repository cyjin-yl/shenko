//! Actual-target native proof producer for the macOS desktop provider
//! (docs/specs/desktop-runtime.md#spec-24-1, docs/specs/desktop-runtime.md#spec-24-3, read under docs/specs/cognition-units.md#spec-51-5, docs/specs/cognition-units.md#spec-51-10).
//!
//! This file is a *consumer*: it drives the real `MacOsDesktopProvider`
//! through the public `DesktopProvider` trait and then checks the effect with
//! independent CoreGraphics observations — `CGEventSourceKeyState` (the HID
//! system state the OS keeps, not this backend's record), `CGEventGetLocation`
//! for the pointer, and `AXIsProcessTrusted` for the Accessibility fact. It
//! never reads provider source text, never treats the provider's own receipt
//! as the effect, and never invents a grant the OS did not give.
//!
//! Honest classification, kept explicit in every case:
//! - **own injected** input: `CGEventPost` from a private event source; the
//!   provider's passive tap must count it as synthetic and the lease epoch
//!   must not move.
//! - **other injected external** input: an event built from a HID-system-state
//!   source and posted by this proof. Whether the OS reports that event as
//!   `kCGEventSourceStateHIDSystemState` is read back from the event itself
//!   before it is posted; only then is a revocation expected. It is still
//!   synthetic, so it is never called physical-human proof.
//! - **OS-reported non-injected / physical HID** input needs a person at this
//!   console and is always reported `NATIVE-UNOBSERVED`; nothing here forces a
//!   pass.
//!
//! Real OS limits stay facts (docs/specs/cognition-units.md#spec-51-10), not approval gates: a missing
//! Screen Recording grant or an untrusted Accessibility client is recorded with
//! the exact typed reason the provider returns, and the proof stops there
//! instead of substituting a synthetic surface.
//!
//! Known platform limit of this producer: macOS gives no query for "is that
//! other process's event tap still registered", so tap teardown is proved by
//! (a) provider drop returning inside a bound — it joins the tap and dispatcher
//! threads and invalidates the port first — and (b) a HID-source event posted
//! afterwards producing no dispatch. That is stated, not overstated.
//!
//! Run on a dedicated macOS console with nobody at it: a real person's
//! keystroke IS external takeover, so genuine human input during a case is
//! attributed as takeover rather than assumed absent.
//!
//! Target command (parent only, currently UNRUN):
//! `cargo test -p shenko-exec-host --test desktop_native_macos_proof -- --test-threads=1 --nocapture`

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shenko_exec_host::{
    DesktopAction, DesktopCapability, DesktopCapabilityFact, DesktopCoordinateTransform,
    DesktopDisplay, DesktopError, DesktopFrame, DesktopInjectOutcome, DesktopInjectRequest,
    DesktopLeaseOp, DesktopLeaseOutcome, MacOsDesktopProvider, DesktopObservation,
    DesktopOpenRequest, DesktopPress, DesktopProvider, DesktopSessionHandle, HumanInputFanout,
    HumanInputObserver, hex_digest,
};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef, SchemaRef},
};

// ---------------------------------------------------------------------------
// CoreGraphics / ApplicationServices surface this proof observes with
// ---------------------------------------------------------------------------

type CgEventRef = *mut c_void;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

/// `kCGEventSourceStateHIDSystemState`: the state the OS reports for the real
/// HID system. This is the value the provider's tap classifies as external.
const CG_EVENT_SOURCE_STATE_HID_SYSTEM: i32 = 1;
/// `kCGHIDEventTap`.
const CG_HID_EVENT_TAP: u32 = 0;
/// `kCGEventSourceStateID`, the event field that carries the source state.
const CG_EVENT_SOURCE_STATE_ID: u32 = 45;
/// ANSI `kVK_Shift`.
const KEY_SHIFT: u16 = 0x38;
/// ANSI `kVK_Control`.
const KEY_CONTROL: u16 = 0x3B;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventCreate(source: *mut c_void) -> CgEventRef;
    fn CGEventGetLocation(event: CgEventRef) -> CgPoint;
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CgEventRef;
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventPost(tap: u32, event: CgEventRef);
    fn CGEventGetIntegerValueField(event: CgEventRef, field: u32) -> i64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *mut c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

// ---------------------------------------------------------------------------
// Honest observation record
// ---------------------------------------------------------------------------

fn observed(case: &str, fact: impl std::fmt::Display) {
    println!("NATIVE-OBSERVED {case}: {fact}");
}

fn unobserved(case: &str, reason: &str) {
    println!("NATIVE-UNOBSERVED {case}: {reason}");
}

/// Human-input dispatch probe: the same public `HumanInputFanout` a browser or
/// mobile peer registers on, so a revocation that never reached the fanout is
/// visible here as an absence.
#[derive(Default)]
struct DispatchProbe {
    hits: Mutex<Vec<String>>,
}

impl DispatchProbe {
    fn count(&self) -> usize {
        self.hits.lock().map(|hits| hits.len()).unwrap_or(0)
    }

    fn domains(&self) -> Vec<String> {
        self.hits.lock().map(|hits| hits.clone()).unwrap_or_default()
    }
}

impl HumanInputObserver for DispatchProbe {
    fn on_human_input(&self, domain_id: &str) {
        if let Ok(mut hits) = self.hits.lock() {
            hits.push(domain_id.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// OS queries
// ---------------------------------------------------------------------------

/// The HID system state the OS keeps for one key, independent of what any
/// backend claims to have sent.
fn hid_key_state(key: u16) -> bool {
    // SAFETY: pure state query, value arguments only.
    unsafe { CGEventSourceKeyState(CG_EVENT_SOURCE_STATE_HID_SYSTEM, key) }
}

fn pointer_position() -> Option<(f64, f64)> {
    // SAFETY: the event is released on the same path before returning.
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return None;
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Some((point.x, point.y))
    }
}

fn accessibility_trusted() -> bool {
    // SAFETY: pure trust query.
    unsafe { AXIsProcessTrusted() }
}

/// Post an event this backend did not create, from a HID-system-state source,
/// and report the source state the OS actually stamped on it. A key-up for a
/// key nobody pressed types nothing and runs nothing.
fn post_hid_source_key_up(key: u16) -> Option<i64> {
    // SAFETY: both objects are created, posted, queried and released here; the
    // null checks run before any use.
    unsafe {
        let source = CGEventSourceCreate(CG_EVENT_SOURCE_STATE_HID_SYSTEM);
        let event = CGEventCreateKeyboardEvent(source, key, false);
        if event.is_null() {
            if !source.is_null() {
                CFRelease(source);
            }
            return None;
        }
        let state = CGEventGetIntegerValueField(event, CG_EVENT_SOURCE_STATE_ID);
        CGEventPost(CG_HID_EVENT_TAP, event);
        CFRelease(event);
        if !source.is_null() {
            CFRelease(source);
        }
        Some(state)
    }
}

fn wait_until(deadline: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if probe() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

// ---------------------------------------------------------------------------
// Provider driving helpers
// ---------------------------------------------------------------------------

fn capability(facts: &[DesktopCapabilityFact], want: DesktopCapability) -> DesktopCapabilityFact {
    facts
        .iter()
        .find(|fact| fact.capability == want)
        .cloned()
        .unwrap_or(DesktopCapabilityFact {
            capability: want,
            available: false,
            detail: "the provider declared no fact for this capability".to_string(),
        })
}

/// The watch facts the live session really reports (tap readiness, trust,
/// counters, last error) rather than a field this proof cannot see.
fn watch_detail(display: &DesktopDisplay) -> serde_json::Value {
    display.detail.clone()
}

fn counter(display: &DesktopDisplay, name: &str) -> u64 {
    watch_detail(display)["external_input_detection"][name]
        .as_u64()
        .unwrap_or(0)
}

fn owned_instance() -> EntityId {
    EntityId::new()
}

fn acquire(
    provider: &dyn DesktopProvider,
    handle: &DesktopSessionHandle,
    instance: EntityId,
) -> Result<DesktopLeaseOutcome, DesktopError> {
    provider.lease_op(handle, DesktopLeaseOp::Acquire, None, instance)
}

fn publish(
    provider: &dyn DesktopProvider,
    handle: &DesktopSessionHandle,
    observation: &DesktopObservation,
    lease_token: EntityId,
    actions: Vec<DesktopAction>,
) -> Result<DesktopInjectOutcome, DesktopError> {
    provider.inject(
        handle,
        DesktopInjectRequest {
            lease_token,
            actions,
            observation: observation.clone(),
            require_pixel_fresh: false,
            checkpoint: Box::new(|| Ok(())),
        },
        EntityId::new(),
    )
}

/// Build the observation an input batch must be bound to, from the frame the
/// provider really captured. The two object refs carry the real content digest
/// and byte size of the captured frames; this producer has no product Store and
/// seals nothing, so the refs say that instead of inventing a CAS location.
fn observation_from(handle: &DesktopSessionHandle, frame: &DesktopFrame) -> DesktopObservation {
    let raw_ref = ObjectRef {
        digest: Digest::blake3(&frame.raw_pixels),
        bytes: frame.raw_pixels.len() as u64,
        media_type: format!("application/x-{}", frame.pixel_format.name),
        store_namespace: "native-proof-unsealed".to_string(),
        locator: "target-native-proof".to_string(),
        codec: SchemaRef {
            name: "computer-use-pixels".to_string(),
            major: 1,
            digest: Digest::blake3(b"computer-use-pixels/v1"),
        },
    };
    let preview_ref = ObjectRef {
        digest: Digest::blake3(&frame.rgb),
        bytes: frame.rgb.len() as u64,
        media_type: "image/rgb8-preview".to_string(),
        store_namespace: "native-proof-unsealed".to_string(),
        locator: "target-native-proof".to_string(),
        codec: SchemaRef {
            name: "computer-use-preview".to_string(),
            major: 1,
            digest: Digest::blake3(b"computer-use-preview/v1"),
        },
    };
    let display = handle.session.display();
    DesktopObservation {
        session_id: handle.session_id,
        provider: display.provider,
        domain_id: display.domain_id,
        display: display.display_id.clone(),
        display_id: display.display_id,
        screen_num: frame.screen_num,
        frame_id: frame.frame_id,
        revision: frame.revision,
        captured_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0),
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
        png_preview_ref: preview_ref,
    }
}

fn fresh_observation(
    provider: &dyn DesktopProvider,
    handle: &DesktopSessionHandle,
) -> Option<DesktopObservation> {
    match provider.observe(handle, None) {
        Ok(frame) => Some(observation_from(handle, &frame)),
        Err(error) => {
            unobserved("observe", &format!("{error}"));
            None
        }
    }
}

/// Open a session, or record the real permission state and stop. macOS refuses
/// a session without the Screen Recording grant; that refusal is the honest
/// permission-denied observation, not something to route around.
fn open_or_report(
    provider: &dyn DesktopProvider,
    case: &str,
    instance: EntityId,
) -> Option<DesktopSessionHandle> {
    match provider.open(&DesktopOpenRequest::default(), instance) {
        Ok(handle) => Some(handle),
        Err(error) => {
            unobserved(
                case,
                &format!(
                    "the OS refused a real session ({}): {}",
                    error.reason(),
                    error.detail()
                ),
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Case 1: permission state, tap readiness and a real capture
// ---------------------------------------------------------------------------

/// Opening a session exercises the real capture path, so a missing Screen
/// Recording grant is refused with the exact typed capability; with the grant,
/// the tap's own readiness (Accessibility trust, tap creation, run-loop source)
/// is what the session reports, and a real frame comes back.
#[test]
fn macos_permission_state_and_tap_readiness() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(MacOsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let instance = owned_instance();
    let Some(handle) = open_or_report(provider.as_ref(), "screen-reading-grant", instance) else {
        // The refusal above is the observation: without the grant this console
        // has no capturable display, and no synthetic surface stands in for it.
        observed(
            "accessibility-trust",
            format!("AXIsProcessTrusted = {}", accessibility_trusted()),
        );
        return;
    };

    let display = handle.session.display();
    assert_eq!(display.provider, "macos");
    assert_eq!(display.session_kind, "cg_display");
    assert!(display.domain_id.starts_with("macos/"));
    assert!(display.width_px > 0 && display.height_px > 0);
    assert_eq!(handle.owner_instance, instance);
    let trust = watch_detail(&display)["accessibility_trusted"]
        .as_bool()
        .unwrap_or(false);
    observed(
        "screen-recording-grant",
        format!(
            "a real {}x{} session opened; AXIsProcessTrusted = {trust}",
            display.width_px, display.height_px
        ),
    );

    let facts = provider.capabilities();
    for want in [
        DesktopCapability::ScreenRead,
        DesktopCapability::Input,
        DesktopCapability::LayoutRevision,
        DesktopCapability::PixelFreshness,
    ] {
        let fact = capability(&facts, want);
        assert!(fact.available, "{want:?} must be available: {}", fact.detail);
    }
    let detection = capability(&facts, DesktopCapability::ExternalInputDetection);
    if detection.available {
        assert_eq!(counter(&display, "tap_created"), 1);
        assert_eq!(counter(&display, "run_loop_source_added"), 1);
        observed(
            "tap-readiness",
            format!(
                "passive tap installed on an owned run-loop source; Accessibility trusted = {trust}"
            ),
        );
    } else {
        unobserved(
            "tap-readiness",
            &format!("the tap did not install: {}", detection.detail),
        );
        assert!(
            !detection.detail.is_empty(),
            "a missing tap must name its own failure"
        );
    }

    let frame = provider.observe(&handle, None).expect("capture");
    assert_eq!(frame.pixel_format.name, "bgra8");
    assert_eq!(frame.pixel_format.bits_per_pixel, 32);
    assert_eq!(
        frame.raw_pixels.len() as u32,
        frame.width * frame.height * 4,
        "the raw pixels are the provider's native 32-bit layout"
    );
    assert_eq!(frame.rgb.len() as u32, frame.width * frame.height * 3);
    assert!(!frame.layout_fingerprint.is_empty());
    assert_eq!(frame.lease_epoch, 0, "a fresh domain has no revocation yet");
    observed(
        "capture",
        format!(
            "{}x{} bgra8 {} bytes",
            frame.width,
            frame.height,
            frame.raw_pixels.len()
        ),
    );
    assert_eq!(probe.count(), 0, "opening a session is not external input");
    provider.close(&handle, instance).expect("close");
}

// ---------------------------------------------------------------------------
// Case 2: own input does not revoke, and the OS shows the effect
// ---------------------------------------------------------------------------

/// Pointer and key input this backend injected must not revoke its own epoch,
/// and the OS must show the effect: the pointer really moved (CoreGraphics
/// reports the location) and the HID keyboard state really held the key down
/// and then up. Text is reported unobserved because typing it would land in an
/// application this proof does not own.
#[test]
fn macos_own_input_does_not_revoke_and_is_os_observed() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(MacOsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let instance = owned_instance();
    let Some(handle) = open_or_report(provider.as_ref(), "own-input", instance) else {
        return;
    };
    let original_pointer = pointer_position();
    let lease = acquire(provider.as_ref(), &handle, instance).expect("acquire");
    assert_eq!(lease.result, "acquired");
    let token = lease.lease_token.expect("an acquired lease has a token");
    let epoch = lease.epoch;
    let synthetic_before = counter(&handle.session.display(), "synthetic_events_filtered");
    let Some(observation) = fresh_observation(provider.as_ref(), &handle) else {
        return;
    };

    let display = handle.session.display();
    let width = i32::from(display.width_px);
    let height = i32::from(display.height_px);
    let origin = original_pointer.unwrap_or((0.0, 0.0));
    let target = (
        (origin.0 + 17.0).clamp(1.0, f64::from(width - 2)),
        (origin.1 + 13.0).clamp(1.0, f64::from(height - 2)),
    );
    let moved = publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::PointerMove {
            x: target.0 as i32,
            y: target.1 as i32,
        }],
    )
    .expect("pointer batch");
    assert_eq!(moved.stopped_reason, None);
    assert_eq!(moved.receipts.len(), 1);
    assert_eq!(moved.receipts[0].transport, "acked");
    let reported = pointer_position().expect("pointer location");
    assert_eq!(
        (reported.0 as i32, reported.1 as i32),
        (target.0 as i32, target.1 as i32),
        "CGEventGetLocation must report the injected pointer position"
    );
    observed("own-pointer", format!("pointer moved to {reported:?}"));

    // Own held key: with Accessibility trust the HID system state must show it
    // down and then up. Without trust the same read is a permission fact, so it
    // is reported rather than asserted.
    let trusted = accessibility_trusted();
    let held = publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::Key {
            key: "shift".to_string(),
            press: Some(DesktopPress::Down),
        }],
    )
    .expect("key down");
    assert_eq!(held.stopped_reason, None);
    if trusted {
        assert!(
            hid_key_state(KEY_SHIFT),
            "CGEventSourceKeyState must independently report the held shift key"
        );
        observed("held-input-down", "CGEventSourceKeyState(HID, shift) = true");
    } else {
        unobserved(
            "held-input-down",
            "reading the HID key state needs Accessibility trust; AXIsProcessTrusted = false",
        );
    }
    let released = publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::Key {
            key: "shift".to_string(),
            press: Some(DesktopPress::Up),
        }],
    )
    .expect("key up");
    assert_eq!(released.stopped_reason, None);
    if trusted {
        assert!(
            !hid_key_state(KEY_SHIFT),
            "the released key must not stay reported down"
        );
    }

    unobserved(
        "own-text",
        "text input needs a window this proof owns; macOS gives the focused application the characters, and typing into another application is not part of this proof",
    );

    // Nothing above may have revoked the writer epoch.
    let after = fresh_observation(provider.as_ref(), &handle).expect("observation");
    assert_eq!(
        after.lease_epoch_at_capture, epoch,
        "own injected input must not revoke the lease epoch (a real person at this console would)"
    );
    assert_eq!(
        probe.count(),
        0,
        "own injected input is not external input (a real person at this console would be)"
    );
    let synthetic_after = counter(&handle.session.display(), "synthetic_events_filtered");
    if synthetic_after > synthetic_before {
        observed(
            "own-input-classification",
            format!(
                "the tap counted {} own private-source events; epoch stayed {epoch}",
                synthetic_after - synthetic_before
            ),
        );
    } else {
        unobserved(
            "own-input-classification",
            "the tap was not installed, so own-event classification was not observable",
        );
    }

    // Put the pointer back where this proof found it, while the lease this
    // case took is still the live writer.
    if let Some((x, y)) = original_pointer {
        publish(
            provider.as_ref(),
            &handle,
            &after,
            token,
            vec![DesktopAction::PointerMove {
                x: x as i32,
                y: y as i32,
            }],
        )
        .expect("restoring the pointer");
    }
    let released_lease = provider
        .lease_op(&handle, DesktopLeaseOp::Release, Some(token), instance)
        .expect("release");
    assert_eq!(released_lease.result, "released");
    provider.close(&handle, instance).expect("close");
}

// ---------------------------------------------------------------------------
// Case 3: takeover revokes the epoch, releases held input, fences old frames
// ---------------------------------------------------------------------------

/// A takeover — the explicit takeover operation, and a HID-source event this
/// proof posts when the OS really stamps it as HID system state — must revoke
/// the original epoch, release machine-held input, and make the pre-takeover
/// frame's action refused with no effect.
#[test]
fn macos_takeover_revokes_epoch_and_fences_old_frames() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(MacOsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let instance = owned_instance();
    let Some(handle) = open_or_report(provider.as_ref(), "takeover", instance) else {
        return;
    };
    let display = handle.session.display();
    let detection = capability(
        &provider.capabilities(),
        DesktopCapability::ExternalInputDetection,
    );

    // Part A: an event from a HID-system-state source. Whether the OS stamps it
    // as HID system state is read back from the event, so an unobservable case
    // is reported rather than assumed.
    let lease = acquire(provider.as_ref(), &handle, instance).expect("acquire");
    let token = lease.lease_token.expect("token");
    let epoch = lease.epoch;
    let observation = fresh_observation(provider.as_ref(), &handle).expect("observation");
    publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::Key {
            key: "shift".to_string(),
            press: Some(DesktopPress::Down),
        }],
    )
    .expect("key down");
    if accessibility_trusted() {
        assert!(hid_key_state(KEY_SHIFT), "the held key must be down in the HID state");
    } else {
        unobserved(
            "held-input-down",
            "reading the HID key state needs Accessibility trust; AXIsProcessTrusted = false",
        );
    }
    let before_rejection = pointer_position().expect("pointer");
    let source_state = post_hid_source_key_up(KEY_CONTROL);
    match source_state {
        Some(state) if state == i64::from(CG_EVENT_SOURCE_STATE_HID_SYSTEM) => {
            assert!(
                detection.available,
                "the tap must be installed to classify a HID-source event"
            );
            assert!(
                wait_until(Duration::from_secs(3), || counter(
                    &handle.session.display(),
                    "external_events"
                ) > 0),
                "the tap must report the HID-source event as external"
            );
            assert!(
                wait_until(Duration::from_secs(2), || probe.count() > 0),
                "the revocation must reach the shared human-input fanout"
            );
            assert!(
                probe
                    .domains()
                    .iter()
                    .all(|domain| *domain == display.domain_id),
                "the fanout carries the display domain that was taken over: {:?}",
                probe.domains()
            );
            assert!(
                wait_until(Duration::from_secs(2), || !hid_key_state(KEY_SHIFT)),
                "the takeover must release the machine-held key"
            );
            let after = fresh_observation(provider.as_ref(), &handle).expect("observation");
            assert!(
                after.lease_epoch_at_capture > epoch,
                "a HID-source takeover must bump the domain epoch"
            );
            observed(
                "external-hid-source-takeover",
                format!(
                    "source state {state} classified external; epoch {epoch} -> {}; held key released",
                    after.lease_epoch_at_capture
                ),
            );
            // The pre-takeover frame is fenced before any event.
            match publish(
                provider.as_ref(),
                &handle,
                &observation,
                token,
                vec![DesktopAction::PointerMove {
                    x: before_rejection.0 as i32 + 40,
                    y: before_rejection.1 as i32 + 40,
                }],
            ) {
                Err(error) => assert_eq!(error.reason(), "lease_epoch_changed"),
                Ok(outcome) => panic!(
                    "a pre-takeover batch must be refused, got stopped_reason {:?}",
                    outcome.stopped_reason
                ),
            }
            let now = pointer_position().expect("pointer");
            assert_eq!(
                (now.0 as i32, now.1 as i32),
                (before_rejection.0 as i32, before_rejection.1 as i32),
                "a refused batch must not move the pointer"
            );
        }
        Some(state) => unobserved(
            "external-hid-source-takeover",
            &format!(
                "the OS stamped the posted event with source state {state}, not HID system state, so this backend must not read it as takeover"
            ),
        ),
        None => unobserved(
            "external-hid-source-takeover",
            "the event could not be created from a HID-system-state source",
        ),
    }

    // Part B: the explicit takeover operation, which is deterministic on any
    // host. It must revoke the previous epoch and release held input.
    let _ = provider.lease_op(&handle, DesktopLeaseOp::Release, Some(token), instance);
    let lease = acquire(provider.as_ref(), &handle, instance).expect("acquire");
    let token = lease.lease_token.expect("token");
    let epoch = lease.epoch;
    let observation = fresh_observation(provider.as_ref(), &handle).expect("observation");
    publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::Key {
            key: "shift".to_string(),
            press: Some(DesktopPress::Down),
        }],
    )
    .expect("key down");
    let trusted = accessibility_trusted();
    if trusted {
        assert!(hid_key_state(KEY_SHIFT), "the held key must be down in the HID state");
    }
    let before_rejection = pointer_position().expect("pointer");
    let takeover = provider
        .lease_op(&handle, DesktopLeaseOp::Takeover, None, EntityId::new())
        .expect("takeover");
    assert_eq!(takeover.result, "taken_over");
    assert!(takeover.revoked);
    assert!(
        takeover.epoch > epoch,
        "takeover must bump the epoch: {epoch} -> {}",
        takeover.epoch
    );
    assert!(
        takeover.held_released >= 1,
        "takeover must release the machine-held key"
    );
    if trusted {
        assert!(
            wait_until(Duration::from_secs(2), || !hid_key_state(KEY_SHIFT)),
            "the released key must be up in the HID system state"
        );
    }
    observed(
        "explicit-takeover",
        format!(
            "epoch {epoch} -> {}; held_released={}",
            takeover.epoch, takeover.held_released
        ),
    );

    // The old frame's action is refused and has no effect.
    match publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::PointerMove {
            x: before_rejection.0 as i32 + 40,
            y: before_rejection.1 as i32 + 40,
        }],
    ) {
        Err(error) => assert_eq!(error.reason(), "lease_epoch_changed"),
        Ok(outcome) => panic!(
            "a pre-takeover batch must be refused, got stopped_reason {:?}",
            outcome.stopped_reason
        ),
    }
    // A fresh observation with the revoked token is refused by the lease guard.
    let fresh = fresh_observation(provider.as_ref(), &handle).expect("observation");
    let fenced = publish(
        provider.as_ref(),
        &handle,
        &fresh,
        token,
        vec![DesktopAction::PointerMove {
            x: before_rejection.0 as i32 + 40,
            y: before_rejection.1 as i32 + 40,
        }],
    )
    .expect("the batch is accounted, not a transport error");
    assert_eq!(fenced.stopped_reason.as_deref(), Some("lease_revoked"));
    assert_eq!(fenced.sent_events, 0);
    assert_eq!(fenced.receipts[0].transport, "rejected");
    let now = pointer_position().expect("pointer");
    assert_eq!(
        (now.0 as i32, now.1 as i32),
        (before_rejection.0 as i32, before_rejection.1 as i32),
        "a lease-fenced batch must not move the pointer"
    );
    observed(
        "revoked-token-rejected",
        "the revoked token was refused with no event and no pointer movement",
    );

    unobserved(
        "physical-hid-takeover",
        "a non-injected human keypress needs a person at this console; the injected HID-source event above is not physical-human proof",
    );
    provider.close(&handle, instance).expect("close");
}

// ---------------------------------------------------------------------------
// Case 4: session close, provider teardown, late input
// ---------------------------------------------------------------------------

/// Closing the writer session releases what it held and revokes the epoch while
/// a sibling session survives; dropping the provider returns inside a bound
/// (it joins the tap and dispatcher threads first) and leaves no live
/// classification behind, which a late HID-source event must not reach.
#[test]
fn macos_session_close_and_provider_teardown() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let instance = owned_instance();
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(MacOsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let Some(first) = open_or_report(provider.as_ref(), "session-close", instance) else {
        return;
    };
    let second = provider
        .open(&DesktopOpenRequest::default(), instance)
        .expect("a second session on the same display");
    let display = first.session.display();
    assert_eq!(
        second.session.display().domain_id,
        display.domain_id,
        "sessions on one display share the lease domain"
    );

    let lease = acquire(provider.as_ref(), &first, instance).expect("acquire");
    let token = lease.lease_token.expect("token");
    let observation = fresh_observation(provider.as_ref(), &first).expect("observation");
    publish(
        provider.as_ref(),
        &first,
        &observation,
        token,
        vec![DesktopAction::Key {
            key: "shift".to_string(),
            press: Some(DesktopPress::Down),
        }],
    )
    .expect("key down");
    let trusted = accessibility_trusted();
    if trusted {
        assert!(hid_key_state(KEY_SHIFT), "the held key must be down in the HID state");
    }

    let closed = provider.close(&first, instance).expect("close a");
    assert!(closed.lease_revoked, "closing the writer revokes the epoch");
    assert!(
        closed.held_released >= 1,
        "closing the writer releases held input"
    );
    if trusted {
        assert!(
            wait_until(Duration::from_secs(2), || !hid_key_state(KEY_SHIFT)),
            "the closed session's held key must be up in the HID system state"
        );
    }
    assert!(
        !second.session.is_closed(),
        "a sibling session outlives the close"
    );
    observed(
        "session-close",
        format!(
            "held_released={} lease_revoked={}",
            closed.held_released, closed.lease_revoked
        ),
    );

    // Final teardown: the drop joins the tap and dispatcher threads.
    let started = Instant::now();
    drop(provider);
    let teardown = started.elapsed();
    assert!(
        teardown < Duration::from_secs(10),
        "provider teardown must return after joining its threads, took {teardown:?}"
    );
    observed(
        "tap-teardown",
        format!("provider drop returned in {teardown:?} after joining its threads"),
    );
    if trusted {
        assert!(
            !hid_key_state(KEY_SHIFT),
            "dropping every session releases what it held"
        );
    }

    // Late input after teardown must reach no observer.
    let hits_after_drop = probe.count();
    let late = post_hid_source_key_up(KEY_CONTROL);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        probe.count(),
        hits_after_drop,
        "no retired tap may dispatch after the provider is gone"
    );
    observed(
        "late-input-after-teardown",
        format!(
            "a HID-source event (source state {late:?}) produced no dispatch after teardown"
        ),
    );

    // A fresh provider really installs its own tap.
    let revived: Arc<dyn DesktopProvider> =
        Arc::new(MacOsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let Some(revived_handle) = open_or_report(revived.as_ref(), "revived-provider", instance) else {
        return;
    };
    let revived_detection = capability(
        &revived.capabilities(),
        DesktopCapability::ExternalInputDetection,
    );
    if revived_detection.available {
        observed(
            "tap-reinstall",
            "a provider created after teardown installed a tap again",
        );
    } else {
        unobserved(
            "tap-reinstall",
            &format!("the fresh tap did not install: {}", revived_detection.detail),
        );
    }
    revived.close(&revived_handle, instance).expect("close");
    drop(revived);
    drop(second);
}
