//! Real macOS desktop provider (SPEC §24.1, §24.3).
//!
//! Hand-rolled FFI against the CoreGraphics/ApplicationServices frameworks
//! — no external objc2/core-graphics crates — so the implementation stays
//! inside this crate's dependency surface. Observation is a genuine
//! `CGDisplayCreateImage` capture; input is genuine `CGEventPost` HID
//! injection. One write lease per display domain uses the same neutral
//! `apply_lease_op` algorithm as the other providers.
//!
//! Honest limits, never a blanket rejection: capture needs the macOS Screen
//! Recording privacy grant (absence is a typed-unavailable with the exact
//! missing capability). Human-input detection is a passive `CGEventTap` that
//! this provider installs on its own thread with an owned CFRunLoop source;
//! its real readiness (Accessibility trust, tap creation, source addition,
//! OS disables) is reported per session, and its tap is invalidated and its
//! thread joined before the provider is gone, so no callback outlives state.

#![cfg(target_os = "macos")]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use shenko_store::EntityId;

use crate::desktop::*;

// ---------------------------------------------------------------------------
// CoreGraphics / ApplicationServices FFI (opaque handles, x86_64/arm64)
// ---------------------------------------------------------------------------

type CgImageRef = *mut c_void;
type CgDataProviderRef = *mut c_void;
type CfDataRef = *mut c_void;
type CgEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFStringRef = *const c_void;
type CGEventTapProxy = *mut c_void;
type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, u32, CgEventRef, *mut c_void) -> CgEventRef;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgRect {
    origin: CgPoint,
    size: CgSize,
}

const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const K_CG_EVENT_KEY_DOWN: u32 = 10;
const K_CG_EVENT_KEY_UP: u32 = 11;
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;
const K_CG_HID_EVENT_TAP: u32 = 0;
/// CGEventField kCGMouseEventClickState.
const K_CG_MOUSE_EVENT_CLICK_STATE: u32 = 18;
// Event-tap constants and the source-state field, from CoreGraphics'
// CGEventTypes.h and IOLLEvent.h (see the `docs/specs/desktop-runtime.md`
// source list): a passive listener at the session tap, and the CGEventField
// that reports which source state created an event.
const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
/// CGEventField kCGEventSourceStateID.
const K_CG_EVENT_SOURCE_STATE_ID: u32 = 45;
/// CGEventSourceStateID kCGEventSourceStateHIDSystemState: a real hardware
/// device produced the event.
const K_CG_EVENT_SOURCE_STATE_HID_SYSTEM: i64 = 1;
/// CGEventSourceStateID kCGEventSourceStatePrivate, used for this backend's
/// own injected events so they never look like HID input.
const K_CG_EVENT_SOURCE_STATE_PRIVATE: i32 = -1;
/// IOLLEvent.h: NX_FLAGSCHANGED, NX_SCROLLWHEELMOVED, NX_OTHERMOUSE*.
const K_CG_EVENT_FLAGS_CHANGED: u32 = 12;
const K_CG_EVENT_SCROLL_WHEEL: u32 = 22;
const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGDisplayBounds(display: u32) -> CgRect;
    fn CGDisplayScreenSize(display: u32) -> CgSize;
    fn CGDisplayCreateImage(display: u32) -> CgImageRef;
    fn CGImageGetWidth(image: CgImageRef) -> usize;
    fn CGImageGetHeight(image: CgImageRef) -> usize;
    fn CGImageGetBytesPerRow(image: CgImageRef) -> usize;
    fn CGImageGetDataProvider(image: CgImageRef) -> CgDataProviderRef;
    fn CGDataProviderCopyData(provider: CgDataProviderRef) -> CfDataRef;
    fn CFDataGetBytePtr(data: CfDataRef) -> *const u8;
    fn CFDataGetLength(data: CfDataRef) -> usize;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        event_type: u32,
        position: CgPoint,
        button: u32,
    ) -> CgEventRef;
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CgEventRef;
    fn CGEventKeyboardSetUnicodeString(
        event: CgEventRef,
        string_length: usize,
        unicode: *const u16,
    );
    fn CGEventSetIntegerValueField(event: CgEventRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CgEventRef);
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CgEventRef, field: u32) -> i64;
    /// A private event source: events created from it report
    /// `kCGEventSourceStatePrivate`, never the HID system state, so this
    /// backend's own injections can never be classified as human input.
    fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
}

/// CoreFoundation owns the run loop and the mach port; `CFRelease` is a
/// CoreFoundation symbol and is declared here so the framework that defines
/// it is the one the linker binds.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *mut c_void);
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(
        run_loop: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: CFStringRef,
    );
    fn CFRunLoopRemoveSource(
        run_loop: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: CFStringRef,
    );
    fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
    fn CFRunLoopStop(run_loop: CFRunLoopRef);
    fn CFRunLoopWakeUp(run_loop: CFRunLoopRef);
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFRunLoopDefaultMode: CFStringRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

// ---------------------------------------------------------------------------
// Key names to macOS virtual keycodes (real US ANSI subset; anything else
// is rejected by name, never guessed).
// ---------------------------------------------------------------------------

fn virtual_key_code(name: &str) -> Option<u16> {
    let upper = name.to_ascii_uppercase();
    let code = match upper.as_str() {
        "A" => 0x00,
        "S" => 0x01,
        "D" => 0x02,
        "F" => 0x03,
        "H" => 0x04,
        "G" => 0x05,
        "Z" => 0x06,
        "X" => 0x07,
        "C" => 0x08,
        "V" => 0x09,
        "B" => 0x0B,
        "Q" => 0x0C,
        "W" => 0x0D,
        "E" => 0x0E,
        "R" => 0x0F,
        "Y" => 0x10,
        "T" => 0x11,
        "1" | "EXCLAM" => 0x12,
        "2" | "AT" => 0x13,
        "3" | "NUMBERSIGN" => 0x14,
        "4" | "DOLLAR" => 0x15,
        "6" | "CARET" => 0x16,
        "5" | "PERCENT" => 0x17,
        "EQUAL" => 0x18,
        "9" | "PARENLEFT" => 0x19,
        "7" | "AMPERSAND" => 0x1A,
        "MINUS" => 0x1B,
        "8" | "ASTERISK" => 0x1C,
        "0" | "PARENRIGHT" => 0x1D,
        "RIGHTBRACKET" => 0x1E,
        "O" => 0x1F,
        "U" => 0x20,
        "LEFTBRACKET" => 0x21,
        "I" => 0x22,
        "P" => 0x23,
        "RETURN" | "ENTER" | "KP_ENTER" => 0x24,
        "L" => 0x25,
        "J" => 0x26,
        "QUOTEDBL" | "APOSTROPHE" => 0x27,
        "K" => 0x28,
        "SEMICOLON" => 0x29,
        "BACKSLASH" => 0x2A,
        "COMMA" => 0x2B,
        "SLASH" => 0x2C,
        "N" => 0x2D,
        "M" => 0x2E,
        "PERIOD" => 0x2F,
        "TAB" => 0x30,
        "SPACE" => 0x31,
        "GRAVE" => 0x32,
        "DELETE" | "BACKSPACE" => 0x33,
        "ESCAPE" => 0x35,
        "COMMAND" | "LMETA" | "LSUPER" => 0x37,
        "SHIFT" | "LSHIFT" => 0x38,
        "CAPSLOCK" => 0x39,
        "OPTION" | "ALT" | "LALT" => 0x3A,
        "CTRL" | "LCTRL" | "CONTROL" => 0x3B,
        "RSHIFT" => 0x3C,
        "ROPTION" | "RALT" => 0x3D,
        "RCTRL" => 0x3E,
        "FN" => 0x3F,
        "F17" => 0x40,
        "KP_DECIMAL" => 0x41,
        "KP_MULTIPLY" => 0x43,
        "KP_ADD" => 0x45,
        "NUMLOCK" | "CLEAR" => 0x47,
        "KP_DIVIDE" => 0x4B,
        "KP_SUBTRACT" => 0x4E,
        "F18" => 0x4F,
        "F19" => 0x50,
        "KP_EQUAL" => 0x51,
        "KP_0" => 0x52,
        "KP_1" => 0x53,
        "KP_2" => 0x54,
        "KP_3" => 0x55,
        "KP_4" => 0x56,
        "KP_5" => 0x57,
        "KP_6" => 0x58,
        "KP_7" => 0x59,
        "F20" => 0x5A,
        "KP_8" => 0x5B,
        "KP_9" => 0x5C,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F3" => 0x63,
        "F8" => 0x64,
        "F9" => 0x65,
        "F11" => 0x67,
        "F13" => 0x69,
        "F16" => 0x6A,
        "F14" => 0x6B,
        "F10" => 0x6D,
        "F12" => 0x6F,
        "F15" => 0x71,
        "HOME" => 0x73,
        "PAGEUP" | "PRIOR" => 0x74,
        "FORWARDDELETE" | "DELETE_RIGHT" => 0x75,
        "F4" => 0x76,
        "END" => 0x77,
        "F2" => 0x78,
        "PAGEDOWN" | "NEXT" => 0x79,
        "F1" => 0x7A,
        "LEFT" => 0x7B,
        "RIGHT" => 0x7C,
        "DOWN" => 0x7D,
        "UP" => 0x7E,
        _ => return None,
    };
    Some(code)
}

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HeldInput {
    Key(u16),
    Button(u8),
}

struct MacSessionShared {
    next_frame: u64,
    next_revision: u64,
    send_log: BTreeMap<EntityId, usize>,
}

struct MacDomainState {
    lease_state: DesktopLeaseState,
    held: BTreeSet<HeldInput>,
    session_ids: BTreeSet<EntityId>,
}

struct MacSession {
    session_id: EntityId,
    closed: AtomicBool,
    owner_instance: EntityId,
    display_id: u32,
    domain: Arc<Mutex<MacDomainState>>,
    shared: Arc<Mutex<MacSessionShared>>,
    /// The provider's tap watcher; its real readiness is the capability fact
    /// this session reports.
    watch: Arc<HumanInputWatch>,
    width_px: usize,
    height_px: usize,
    dpi: f64,
    trusted: bool,
}

impl MacSession {
    fn locked(&self) -> Result<MutexGuard<'_, MacSessionShared>, DesktopError> {
        self.shared
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))
    }

    fn domain_locked(&self) -> Result<MutexGuard<'_, MacDomainState>, DesktopError> {
        self.domain
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))
    }

    fn lease_guard(
        &self,
        token: EntityId,
        epoch: u64,
    ) -> Result<MutexGuard<'_, MacDomainState>, &'static str> {
        let domain = self.domain.lock().map_err(|_| "backend_state_poisoned")?;
        if self.closed.load(Ordering::Relaxed) {
            return Err("session_closed");
        }
        if domain.lease_state.matches(token, self.session_id, epoch) {
            Ok(domain)
        } else {
            Err("lease_revoked")
        }
    }
}

impl DesktopSession for MacSession {
    fn session_id(&self) -> EntityId {
        self.session_id
    }

    fn provider_id(&self) -> &'static str {
        "macos"
    }

    fn domain_id(&self) -> String {
        format!("macos/{}", self.display_id)
    }

    fn display(&self) -> DesktopDisplay {
        let watch = self.watch.snapshot();
        DesktopDisplay {
            provider: "macos".into(),
            domain_id: self.domain_id(),
            display_id: self.display_id.to_string(),
            session_kind: "cg_display".into(),
            width_px: self.width_px as u32,
            height_px: self.height_px as u32,
            dpi_x: self.dpi,
            dpi_y: self.dpi,
            scale_factor: self.dpi / 72.0,
            depth: 32,
            capabilities: vec![
                DesktopCapabilityFact {
                    capability: DesktopCapability::ScreenRead,
                    available: true,
                    detail: "CGDisplayCreateImage (needs Screen Recording privacy grant)".into(),
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::Input,
                    available: true,
                    detail: "CGEventPost to kCGHIDEventTap".into(),
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::ExternalInputDetection,
                    // The tap's own proven readiness, never the mere presence
                    // of the Accessibility grant.
                    available: watch.installed,
                    detail: match (&watch.last_error, watch.accessibility_trusted) {
                        (Some(error), _) => format!(
                            "the passive CGEventTap is not installed: {error}"
                        ),
                        (None, true) => {
                            "passive CGEventTap installed with an owned CFRunLoop source; Accessibility trusted"
                                .to_string()
                        }
                        (None, false) => {
                            "passive CGEventTap installed with an owned CFRunLoop source; Accessibility not granted, so keyboard input may not be observable"
                                .to_string()
                        }
                    },
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::LayoutRevision,
                    available: true,
                    detail: "online display list + per-display bounds hash".into(),
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::PixelFreshness,
                    available: true,
                    detail: "re-capture pixel hash".into(),
                },
            ],
            detail: serde_json::json!({
                "display_id": self.display_id,
                "width_px": self.width_px,
                "height_px": self.height_px,
                "dpi": self.dpi,
                "accessibility_trusted": self.trusted,
                "external_input_detection": watch,
            }),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for MacSession {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Ok(mut domain) = self.domain.lock() {
            domain.session_ids.remove(&self.session_id);
            let holder_gone = domain
                .lease_state
                .holder
                .as_ref()
                .is_some_and(|l| l.holder_session == self.session_id);
            if domain.session_ids.is_empty() || holder_gone {
                domain.lease_state.holder = None;
                domain.lease_state.epoch += 1;
                let held: Vec<HeldInput> = domain.held.iter().copied().collect();
                domain.held.clear();
                release_held(&held);
            }
        }
    }
}

struct MacState {
    sessions: BTreeMap<EntityId, Arc<MacSession>>,
    domains: BTreeMap<String, Arc<Mutex<MacDomainState>>>,
}

pub struct MacOsDesktopProvider {
    /// Shared with the human-input dispatcher through a `Weak`, so the tap
    /// threads never keep the provider (or its sessions) alive.
    state: Arc<Mutex<MacState>>,
    fanout: Arc<HumanInputFanout>,
    /// Created lazily on the first session; dropped (and joined) with the
    /// provider.
    watch: Mutex<Option<Arc<HumanInputWatch>>>,
}

/// Capture one display as (bgra_raw, rgb8, width, height). The caller must
/// have the Screen Recording grant or CGDisplayCreateImage returns null.
fn capture_display(display_id: u32) -> Result<(Vec<u8>, Vec<u8>, usize, usize), DesktopError> {
    let image = unsafe { CGDisplayCreateImage(display_id) };
    if image.is_null() {
        return Err(DesktopError::unavailable(
            "screen_recording_permission_not_granted",
            "CGDisplayCreateImage returned null; grant Screen Recording for this process",
        ));
    }
    let result = (|| -> Result<(Vec<u8>, Vec<u8>, usize, usize), DesktopError> {
        let width = unsafe { CGImageGetWidth(image) };
        let height = unsafe { CGImageGetHeight(image) };
        let bytes_per_row = unsafe { CGImageGetBytesPerRow(image) };
        if width == 0 || height == 0 || bytes_per_row < width * 4 {
            return Err(DesktopError::unavailable(
                "capture_unavailable",
                "CGImage reported a zero-sized or unsupported layout",
            ));
        }
        let provider = unsafe { CGImageGetDataProvider(image) };
        let data = unsafe { CGDataProviderCopyData(provider) };
        if data.is_null() {
            return Err(DesktopError::unavailable(
                "capture_unavailable",
                "CGDataProviderCopyData returned null",
            ));
        }
        let ptr = unsafe { CFDataGetBytePtr(data) };
        let len = unsafe { CFDataGetLength(data) };
        if ptr.is_null() || len < bytes_per_row * height {
            unsafe { CFRelease(data) };
            return Err(DesktopError::unavailable(
                "capture_unavailable",
                "CFData bytes are shorter than the image extent",
            ));
        }
        let raw_slice = unsafe { std::slice::from_raw_parts(ptr, bytes_per_row * height) };
        let mut raw = Vec::with_capacity(width * height * 4);
        let mut rgb = Vec::with_capacity(width * height * 3);
        for row in raw_slice.chunks_exact(bytes_per_row) {
            for px in row[..width * 4].chunks_exact(4) {
                // CGDisplayCreateImage bytes are BGRA8.
                raw.extend_from_slice(px);
                rgb.push(px[2]);
                rgb.push(px[1]);
                rgb.push(px[0]);
            }
        }
        unsafe { CFRelease(data) };
        Ok((raw, rgb, width, height))
    })();
    unsafe { CFRelease(image) };
    result
}

fn display_dpi(display_id: u32, width_px: usize) -> f64 {
    let size = unsafe { CGDisplayScreenSize(display_id) };
    if size.width > 0.0 {
        f64::from(width_px as u32) / (size.width / 25.4)
    } else {
        72.0
    }
}

/// Crop a full-display BGRA/RGB8 capture to a root-coordinate region.
fn crop_to_region(
    full_raw: Vec<u8>,
    full_rgb: Vec<u8>,
    full_width: usize,
    full_height: usize,
    region: [i32; 4],
    full_region: [i32; 4],
) -> Result<(Vec<u8>, Vec<u8>, usize, usize), DesktopError> {
    let [rx, ry, rw, rh] = region;
    if rw <= 0 || rh <= 0 {
        return Err(DesktopError::rejected("region_empty", "empty capture region"));
    }
    let [fx, fy, fw, fh] = full_region;
    if rx < fx || ry < fy || rx + rw > fx + fw || ry + rh > fy + fh {
        return Err(DesktopError::rejected(
            "region_outside_display",
            "the requested region is not fully inside the display bounds",
        ));
    }
    let offset_x = (rx - fx) as usize;
    let offset_y = (ry - fy) as usize;
    let width = rw as usize;
    let height = rh as usize;
    let mut raw = Vec::with_capacity(width * height * 4);
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in 0..height {
        let start = ((offset_y + row) * full_width + offset_x) * 4;
        raw.extend_from_slice(&full_raw[start..start + width * 4]);
        let rgb_start = ((offset_y + row) * full_width + offset_x) * 3;
        rgb.extend_from_slice(&full_rgb[rgb_start..rgb_start + width * 3]);
    }
    let _ = full_height;
    Ok((raw, rgb, width, height))
}

fn layout_fingerprint() -> String {
    let mut displays = [0u32; 16];
    let mut count = 0u32;
    let mut material = Vec::new();
    unsafe {
        if CGGetOnlineDisplayList(16, displays.as_mut_ptr(), &mut count) == 0 {
            for id in displays[..count as usize].iter() {
                let bounds = CGDisplayBounds(*id);
                material.push((
                    *id,
                    [
                        bounds.origin.x,
                        bounds.origin.y,
                        bounds.size.width,
                        bounds.size.height,
                    ],
                ));
            }
        }
    }
    material.sort();
    hex_digest(serde_json::to_vec(&material).unwrap_or_default().as_slice())
}

fn post_event(build: impl FnOnce() -> CgEventRef) -> Result<(), &'static str> {
    let event = build();
    if event.is_null() {
        return Err("transport_lost");
    }
    unsafe {
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event);
    }
    Ok(())
}

fn mouse_event_type(button: u8, down: bool, dragging: bool) -> Option<u32> {
    match (button, down, dragging) {
        (1, true, false) => Some(K_CG_EVENT_LEFT_MOUSE_DOWN),
        (1, false, false) => Some(K_CG_EVENT_LEFT_MOUSE_UP),
        (1, true, true) => Some(K_CG_EVENT_LEFT_MOUSE_DRAGGED),
        (2, true, false) => Some(K_CG_EVENT_RIGHT_MOUSE_DOWN),
        (2, false, false) => Some(K_CG_EVENT_RIGHT_MOUSE_UP),
        (2, true, true) => Some(K_CG_EVENT_RIGHT_MOUSE_DRAGGED),
        _ => None,
    }
}

fn mouse_button(button: u8) -> Option<u32> {
    match button {
        1 => Some(K_CG_MOUSE_BUTTON_LEFT),
        2 => Some(K_CG_MOUSE_BUTTON_RIGHT),
        3 => Some(K_CG_MOUSE_BUTTON_CENTER),
        _ => None,
    }
}

fn pointer_position() -> Option<(f64, f64)> {
    // The CoreGraphics API to read the current location without an event is
    // CGEventCreate + CGEventGetLocation; declaring just what is used here
    // keeps the surface minimal.
    extern "C" {
        fn CGEventCreate(source: *mut c_void) -> CgEventRef;
        fn CGEventGetLocation(event: CgEventRef) -> CgPoint;
    }
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

fn release_held(held: &[HeldInput]) {
    // Release events are this backend's own too: a private source keeps the
    // provider's tap from reading them as a fresh human takeover.
    let source = unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_PRIVATE) };
    for input in held {
        match *input {
            HeldInput::Key(code) => {
                let _ = post_event(|| unsafe {
                    CGEventCreateKeyboardEvent(source, code, false)
                });
            }
            HeldInput::Button(button) => {
                let Some(button_const) = mouse_button(button) else { continue };
                let Some(event_type) = mouse_event_type(button, false, false) else {
                    continue;
                };
                let position = pointer_position().unwrap_or((0.0, 0.0));
                let _ = post_event(|| unsafe {
                    CGEventCreateMouseEvent(source, event_type, CgPoint { x: position.0, y: position.1 }, button_const)
                });
            }
        }
    }
    if !source.is_null() {
        unsafe { CFRelease(source) };
    }
}

// ---------------------------------------------------------------------------
// Human-input observation: owned passive CGEventTap + CFRunLoop source
// ---------------------------------------------------------------------------

/// How long the provider waits for the tap thread to prove installation.
const TAP_SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// The dispatcher wakes at this cadence only to observe the stop flag; tap
/// callbacks themselves never poll or block.
const TAP_DISPATCH_TICK: Duration = Duration::from_millis(50);

/// One external input observation, as the tap classified it.
#[derive(Debug, Clone, Copy)]
struct ExternalInputEvent {
    kind: &'static str,
}

/// The event mask this tap observes: every HID input type this provider can
/// inject, so a human doing the same thing is always seen.
fn tap_event_mask() -> u64 {
    let types = [
        K_CG_EVENT_LEFT_MOUSE_DOWN,
        K_CG_EVENT_LEFT_MOUSE_UP,
        K_CG_EVENT_RIGHT_MOUSE_DOWN,
        K_CG_EVENT_RIGHT_MOUSE_UP,
        K_CG_EVENT_MOUSE_MOVED,
        K_CG_EVENT_LEFT_MOUSE_DRAGGED,
        K_CG_EVENT_RIGHT_MOUSE_DRAGGED,
        K_CG_EVENT_KEY_DOWN,
        K_CG_EVENT_KEY_UP,
        K_CG_EVENT_FLAGS_CHANGED,
        K_CG_EVENT_SCROLL_WHEEL,
        K_CG_EVENT_OTHER_MOUSE_DOWN,
        K_CG_EVENT_OTHER_MOUSE_UP,
        K_CG_EVENT_OTHER_MOUSE_DRAGGED,
    ];
    let mut mask = 0u64;
    for event_type in types {
        mask |= 1u64 << event_type;
    }
    mask
}

fn external_event_kind(event_type: u32) -> &'static str {
    match event_type {
        K_CG_EVENT_KEY_DOWN | K_CG_EVENT_KEY_UP | K_CG_EVENT_FLAGS_CHANGED => "keyboard",
        K_CG_EVENT_SCROLL_WHEEL => "scroll",
        _ => "mouse",
    }
}

/// Real facts about the provider's event tap, reported per session.
#[derive(Debug, Clone, Default, serde::Serialize)]
struct HumanInputWatchFacts {
    installed: bool,
    tap_created: bool,
    run_loop_source_added: bool,
    accessibility_trusted: bool,
    external_events: u64,
    synthetic_events_filtered: u64,
    disabled_by_timeout: u64,
    disabled_by_user_input: u64,
    last_disable_reason: Option<&'static str>,
    last_error: Option<String>,
    last_revoked_domains: usize,
    last_event_kind: Option<&'static str>,
}

/// One provider's tap: the tap thread owns the CFRunLoop (and therefore the
/// callbacks), the dispatcher thread performs the revoke, and the callback
/// holds only a `Weak` to this watch — it can never extend ownership and can
/// never run after the tap is invalidated and its thread joined.
struct HumanInputWatch {
    domains: Weak<Mutex<MacState>>,
    fanout: Arc<HumanInputFanout>,
    sender: mpsc::Sender<ExternalInputEvent>,
    pending: AtomicBool,
    facts: Mutex<HumanInputWatchFacts>,
    ready: AtomicBool,
    stopped: Arc<AtomicBool>,
    /// CFRunLoopRef of the tap thread, valid only until that thread is joined.
    run_loop: AtomicUsize,
    /// CFMachPortRef (the tap). The callback uses it to re-enable after a
    /// timeout disable; it is cleared before the port is released.
    tap: AtomicUsize,
    synthetic_events: AtomicU64,
    tap_thread: Mutex<Option<JoinHandle<()>>>,
    dispatch_thread: Mutex<Option<JoinHandle<()>>>,
}

impl HumanInputWatch {
    /// Start the tap. Every failure becomes a reported fact: capture and
    /// input keep working, and human-input observation honestly reports why
    /// it is unavailable instead of pretending.
    fn start(domains: Weak<Mutex<MacState>>, fanout: Arc<HumanInputFanout>) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel::<ExternalInputEvent>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let watch = Arc::new(Self {
            domains,
            fanout,
            sender,
            pending: AtomicBool::new(false),
            facts: Mutex::new(HumanInputWatchFacts::default()),
            ready: AtomicBool::new(false),
            stopped: Arc::new(AtomicBool::new(false)),
            run_loop: AtomicUsize::new(0),
            tap: AtomicUsize::new(0),
            synthetic_events: AtomicU64::new(0),
            tap_thread: Mutex::new(None),
            dispatch_thread: Mutex::new(None),
        });
        let weak = Arc::downgrade(&watch);
        let spawned = std::thread::Builder::new()
            .name("shenko-macos-event-tap".into())
            .spawn(move || run_tap_thread(weak, ready_tx));
        match spawned {
            Ok(handle) => {
                if let Ok(mut guard) = watch.tap_thread.lock() {
                    *guard = Some(handle);
                }
            }
            Err(error) => {
                watch.record_error(&format!("tap thread spawn failed: {error}"));
                return watch;
            }
        }
        match ready_rx.recv_timeout(TAP_SETUP_TIMEOUT) {
            Ok(Ok(())) => {
                watch.ready.store(true, Ordering::SeqCst);
                if let Ok(mut facts) = watch.facts.lock() {
                    facts.installed = true;
                }
            }
            Ok(Err(error)) => {
                watch.record_error(&error);
                watch.stop();
                return watch;
            }
            Err(_) => {
                watch.record_error(
                    "tap thread did not report readiness; the watcher is not exposed",
                );
                watch.stop();
                return watch;
            }
        }
        let weak_watch = Arc::downgrade(&watch);
        let domains = watch.domains.clone();
        let fanout = Arc::clone(&watch.fanout);
        let stopped = Arc::clone(&watch.stopped);
        let spawned = std::thread::Builder::new()
            .name("shenko-macos-input-dispatch".into())
            .spawn(move || run_dispatch_thread(receiver, domains, fanout, weak_watch, stopped));
        match spawned {
            Ok(handle) => {
                if let Ok(mut guard) = watch.dispatch_thread.lock() {
                    *guard = Some(handle);
                }
            }
            Err(error) => {
                watch.record_error(&format!("input dispatch thread spawn failed: {error}"));
                watch.stop();
            }
        }
        watch
    }

    /// Called from the tap callback: no domain lock, no blocking, one atomic
    /// test so a burst of motion events cannot flood the dispatcher.
    fn signal_external_input(&self, event: ExternalInputEvent) {
        if self.pending.swap(true, Ordering::Relaxed) {
            return;
        }
        if self.sender.send(event).is_err() {
            self.pending.store(false, Ordering::Relaxed);
        }
    }

    fn record_error(&self, error: &str) {
        if let Ok(mut facts) = self.facts.lock() {
            facts.last_error = Some(error.to_string());
            facts.installed = false;
        }
    }

    fn snapshot(&self) -> HumanInputWatchFacts {
        let mut facts = self
            .facts
            .lock()
            .map(|facts| facts.clone())
            .unwrap_or_default();
        facts.synthetic_events_filtered = self.synthetic_events.load(Ordering::Relaxed);
        facts
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Stop the run loop, join the tap thread (which invalidates and releases
    /// the tap), then join the dispatcher. After this returns no callback can
    /// run and no thread of this watcher exists.
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.ready.store(false, Ordering::SeqCst);
        let run_loop = self.run_loop.load(Ordering::SeqCst);
        if run_loop != 0 {
            // Stop + wake so a sleeping run loop returns promptly; the tap
            // thread then tears the tap down before exiting.
            unsafe {
                CFRunLoopStop(run_loop as CFRunLoopRef);
                CFRunLoopWakeUp(run_loop as CFRunLoopRef);
            }
        }
        // The stop flag is checked before the loop starts and on every slice,
        // so this join always completes; it is what proves no callback can run
        // after teardown returns.
        if let Some(handle) = self.tap_thread.lock().ok().and_then(|mut guard| guard.take()) {
            let _ = handle.join();
        }
        if let Some(handle) = self
            .dispatch_thread
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            let _ = handle.join();
        }
        if let Ok(mut facts) = self.facts.lock() {
            facts.installed = false;
        }
    }
}

impl Drop for HumanInputWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Tap thread: create the tap with an owned run loop source, enable it, prove
/// readiness, then run the loop until stopped and tear everything down.
fn run_tap_thread(watch: Weak<HumanInputWatch>, ready: mpsc::Sender<Result<(), String>>) {
    let Some(strong) = watch.upgrade() else {
        let _ = ready.send(Err("watcher retired before the tap thread started".into()));
        return;
    };
    let trusted = unsafe { AXIsProcessTrusted() };
    if let Ok(mut facts) = strong.facts.lock() {
        facts.accessibility_trusted = trusted;
    }
    // The callback gets a Weak, never a strong reference: it cannot keep this
    // backend (or its sessions) alive, and the box is freed only after the
    // tap is invalidated on this thread.
    let user_info = Box::into_raw(Box::new(Weak::clone(&watch)));
    let tap = unsafe {
        CGEventTapCreate(
            K_CG_SESSION_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
            tap_event_mask(),
            tap_callback,
            user_info as *mut c_void,
        )
    };
    if tap.is_null() {
        // SAFETY: no tap exists, so no callback can hold this box.
        drop(unsafe { Box::from_raw(user_info) });
        let error = if trusted {
            "CGEventTapCreate returned null although Accessibility is trusted; the tap cannot be installed"
                .to_string()
        } else {
            "CGEventTapCreate returned null; grant Accessibility for this process".to_string()
        };
        strong.record_error(&error);
        let _ = ready.send(Err(error));
        return;
    }
    strong.tap.store(tap as usize, Ordering::SeqCst);
    let run_loop = unsafe { CFRunLoopGetCurrent() };
    strong.run_loop.store(run_loop as usize, Ordering::SeqCst);
    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
    if source.is_null() {
        unsafe {
            CFMachPortInvalidate(tap);
            CFRelease(tap);
        }
        strong.tap.store(0, Ordering::SeqCst);
        strong.run_loop.store(0, Ordering::SeqCst);
        // SAFETY: the tap never became active, so no callback ran.
        drop(unsafe { Box::from_raw(user_info) });
        let error = "CFMachPortCreateRunLoopSource returned null; no run loop source owns the tap"
            .to_string();
        strong.record_error(&error);
        let _ = ready.send(Err(error));
        return;
    }
    unsafe {
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
    if let Ok(mut facts) = strong.facts.lock() {
        facts.tap_created = true;
        facts.run_loop_source_added = true;
    }
    let _ = ready.send(Ok(()));
    if strong.stopped.load(Ordering::SeqCst) {
        // The provider gave up while the OS was creating the tap: never enter
        // the run loop, tear down immediately.
        unsafe {
            CGEventTapEnable(tap, false);
            CFMachPortInvalidate(tap);
            CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
            CFRelease(source);
            CFRelease(tap);
        }
        strong.tap.store(0, Ordering::SeqCst);
        strong.run_loop.store(0, Ordering::SeqCst);
        // SAFETY: the tap is invalidated; no callback can run.
        drop(unsafe { Box::from_raw(user_info) });
        return;
    }
    // Run the loop in short slices instead of a single blocking `CFRunLoopRun`
    // so the stop flag is observed even if the provider's `CFRunLoopStop`
    // landed before the loop started. Callbacks still run inside the loop.
    loop {
        if strong.stopped.load(Ordering::SeqCst) {
            break;
        }
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true) };
    }
    // The run loop returned: no callback is executing. Disable and invalidate
    // the tap, then detach and release the source and the port.
    unsafe {
        CGEventTapEnable(tap, false);
        CFMachPortInvalidate(tap);
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        CFRelease(source);
        CFRelease(tap);
    }
    strong.tap.store(0, Ordering::SeqCst);
    strong.run_loop.store(0, Ordering::SeqCst);
    // SAFETY: the tap is invalidated and its run loop returned, so no caller
    // can reach this box any more.
    drop(unsafe { Box::from_raw(user_info) });
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CgEventRef,
    user_info: *mut c_void,
) -> CgEventRef {
    if user_info.is_null() {
        return event;
    }
    // SAFETY: the tap thread owns this box and frees it only after the tap is
    // invalidated, so a live callback always sees a valid `Weak`.
    let Some(watch) = (unsafe { (*(user_info as *const Weak<HumanInputWatch>)).upgrade() }) else {
        return event;
    };
    match event_type {
        K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT => {
            // The OS disabled the tap because a callback overran its budget.
            // Nothing here blocks on domain state, so re-enable immediately.
            let tap = watch.tap.load(Ordering::Relaxed);
            if tap != 0 {
                unsafe { CGEventTapEnable(tap as CFMachPortRef, true) };
            }
            if let Ok(mut facts) = watch.facts.lock() {
                facts.disabled_by_timeout += 1;
                facts.last_disable_reason = Some("timeout");
            }
            return event;
        }
        K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT => {
            if let Ok(mut facts) = watch.facts.lock() {
                facts.disabled_by_user_input += 1;
                facts.last_disable_reason = Some("user_input");
            }
            // Deliberately not re-enabled: the user disabled the tap.
            return event;
        }
        _ => {}
    }
    if event.is_null() {
        return event;
    }
    let source_state = unsafe { CGEventGetIntegerValueField(event, K_CG_EVENT_SOURCE_STATE_ID) };
    if source_state == K_CG_EVENT_SOURCE_STATE_HID_SYSTEM {
        watch.signal_external_input(ExternalInputEvent {
            kind: external_event_kind(event_type),
        });
    } else {
        // Our own CGEventPost batches (private source) and other synthetic
        // events: counted, never treated as a takeover.
        watch.synthetic_events.fetch_add(1, Ordering::Relaxed);
    }
    event
}

/// Dispatcher: the only place external input mutates a domain, so the tap
/// callback never takes a lease lock. Weak ownership means a retired provider
/// simply ends the loop.
fn run_dispatch_thread(
    receiver: mpsc::Receiver<ExternalInputEvent>,
    domains: Weak<Mutex<MacState>>,
    fanout: Arc<HumanInputFanout>,
    watch: Weak<HumanInputWatch>,
    stopped: Arc<AtomicBool>,
) {
    while !stopped.load(Ordering::SeqCst) {
        let event = match receiver.recv_timeout(TAP_DISPATCH_TICK) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(watch) = watch.upgrade() {
            watch.pending.store(false, Ordering::SeqCst);
        }
        let Some(state) = domains.upgrade() else {
            break;
        };
        let revoked = revoke_domains_for_external_input(&state, &fanout);
        if let Some(watch) = watch.upgrade() {
            if let Ok(mut facts) = watch.facts.lock() {
                facts.external_events += 1;
                facts.last_revoked_domains = revoked;
                facts.last_event_kind = Some(event.kind);
            }
        }
    }
}

/// Revoke every display domain this provider holds. A session-level tap sees
/// console input that is not attributable to one CGDirectDisplayID, and the
/// human who reached the machine took over the console — so every machine-held
/// domain epoch is invalidated with the same handover fence the X11 watcher
/// uses: epoch bump and held-input release under the domain lock, then the
/// shared fanout dispatch.
fn revoke_domains_for_external_input(
    state: &Mutex<MacState>,
    fanout: &HumanInputFanout,
) -> usize {
    let mut targets: Vec<(String, Arc<Mutex<MacDomainState>>)> = Vec::new();
    if let Ok(locked) = state.lock() {
        for (domain_id, domain) in &locked.domains {
            targets.push((domain_id.clone(), Arc::clone(domain)));
        }
    }
    let mut revoked = 0;
    for (domain_id, domain) in targets {
        let notified = if let Ok(mut domain) = domain.lock() {
            domain.lease_state.holder = None;
            domain.lease_state.epoch += 1;
            let held: Vec<HeldInput> = domain.held.iter().copied().collect();
            domain.held.clear();
            release_held(&held);
            Some(domain_id)
        } else {
            None
        };
        if let Some(domain_id) = notified {
            revoked += 1;
            fanout.dispatch(&domain_id);
        }
    }
    revoked
}

impl MacOsDesktopProvider {
    pub fn new() -> Self {
        Self::with_fanout(Arc::new(HumanInputFanout::new()))
    }

    /// Share the human-input registry with the product backend so a browser
    /// or mobile peer bound to the same display domain reacts to the same
    /// takeover.
    pub fn with_fanout(fanout: Arc<HumanInputFanout>) -> Self {
        Self {
            state: Arc::new(Mutex::new(MacState {
                sessions: BTreeMap::new(),
                domains: BTreeMap::new(),
            })),
            fanout,
            watch: Mutex::new(None),
        }
    }

    /// Start the tap watcher once, retrying after a failed install. Its own
    /// readiness is what a session reports.
    fn ensure_human_input_watch(&self) -> Arc<HumanInputWatch> {
        {
            let guard = match self.watch.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    return HumanInputWatch::start(
                        Arc::downgrade(&self.state),
                        Arc::clone(&self.fanout),
                    )
                }
            };
            if let Some(watch) = guard.as_ref() {
                if watch.is_ready() {
                    return Arc::clone(watch);
                }
            }
        }
        if let Some(previous) = self.watch.lock().ok().and_then(|mut guard| guard.take()) {
            previous.stop();
        }
        let watch = HumanInputWatch::start(Arc::downgrade(&self.state), Arc::clone(&self.fanout));
        if let Ok(mut guard) = self.watch.lock() {
            *guard = Some(Arc::clone(&watch));
        }
        watch
    }

    fn watch_facts(&self) -> HumanInputWatchFacts {
        match self.watch.lock().ok().and_then(|guard| guard.clone()) {
            Some(watch) => watch.snapshot(),
            None => HumanInputWatchFacts::default(),
        }
    }

    fn locked(&self) -> Result<MutexGuard<'_, MacState>, DesktopError> {
        self.state
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))
    }

    fn session(&self, session_id: EntityId) -> Result<Arc<MacSession>, DesktopError> {
        let state = self.locked()?;
        match state.sessions.get(&session_id) {
            Some(session) if !session.closed.load(Ordering::Relaxed) => Ok(Arc::clone(session)),
            Some(_) => Err(DesktopError::unavailable("session_closed", "")),
            None => Err(DesktopError::unavailable("session_unknown", "")),
        }
    }
}

impl Default for MacOsDesktopProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MacOsDesktopProvider {
    fn drop(&mut self) {
        // Joins the tap and dispatcher threads and invalidates the tap before
        // returning, so no callback or thread outlives the provider.
        if let Some(watch) = self.watch.lock().ok().and_then(|mut guard| guard.take()) {
            watch.stop();
        }
    }
}

impl DesktopProvider for MacOsDesktopProvider {
    fn provider_id(&self) -> &'static str {
        "macos"
    }

    fn capabilities(&self) -> Vec<DesktopCapabilityFact> {
        // Discovery reports the tap's REAL state: before the first session no
        // tap exists, so detection is not yet available and this says so.
        let watch = self.watch_facts();
        vec![
            DesktopCapabilityFact {
                capability: DesktopCapability::ScreenRead,
                available: true,
                detail: "CGDisplayCreateImage".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::Input,
                available: true,
                detail: "CGEventPost (kCGHIDEventTap)".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::ExternalInputDetection,
                available: watch.installed,
                detail: if watch.installed {
                    format!(
                        "passive CGEventTap on a session tap with an owned CFRunLoop source (Accessibility trusted: {}); HID-source-state events revoke, this backend's private-source events do not",
                        watch.accessibility_trusted
                    )
                } else {
                    match &watch.last_error {
                        Some(error) => format!(
                            "the tap is installed on the first session open and failed here: {error}"
                        ),
                        None => {
                            "the passive tap is installed on the first session open; none is running yet"
                                .to_string()
                        }
                    }
                },
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::LayoutRevision,
                available: true,
                detail: "online display list + bounds hash".into(),
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
        let display_id = match request.display.as_deref() {
            None | Some("") => unsafe { CGMainDisplayID() },
            Some(raw) => raw.parse::<u32>().map_err(|_| {
                DesktopError::rejected("display_unparseable", "macOS display ids are unsigned integers")
            })?,
        };
        // macOS addresses displays by CGDirectDisplayID above; a screen
        // index would silently select a different display, so it is refused
        // rather than substituted.
        if let Some(screen) = request.screen {
            if screen != 0 {
                return Err(DesktopError::unavailable(
                    "screen_index_unavailable",
                    format!(
                        "screen index {screen} was requested; pass the CGDirectDisplayID as `display` instead"
                    ),
                ));
            }
        }
        // Probe the real capture path at open: without the Screen Recording
        // grant this is a typed-unavailable naming the exact capability.
        let (_, _, width, height) = capture_display(display_id)?;
        let trusted = unsafe { AXIsProcessTrusted() };
        let session_id = EntityId::new();
        // Human-input observation is a sub-capability: start the tap watcher
        // (idempotent) and let its probed readiness decide the fact. A tap
        // that cannot install never refuses the session and never becomes a
        // fake capability.
        let watch = self.ensure_human_input_watch();
        let domain_id = format!("macos/{display_id}");
        let dpi = display_dpi(display_id, width);
        let mut state = self.locked()?;
        let domain = state
            .domains
            .entry(domain_id.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(MacDomainState {
                    lease_state: DesktopLeaseState::new(),
                    held: BTreeSet::new(),
                    session_ids: BTreeSet::new(),
                }))
            })
            .clone();
        state.sessions.insert(
            session_id,
            Arc::new(MacSession {
                session_id,
                closed: AtomicBool::new(false),
                owner_instance,
                display_id,
                domain,
                shared: Arc::new(Mutex::new(MacSessionShared {
                    next_frame: 0,
                    next_revision: 0,
                    send_log: BTreeMap::new(),
                })),
                watch,
                width_px: width,
                height_px: height,
                dpi,
                trusted,
            }),
        );
        drop(state);
        let session = self.session(session_id)?;
        if let Ok(mut domain) = session.domain_locked() {
            domain.session_ids.insert(session_id);
        }
        let session: Arc<dyn DesktopSession> = session;
        Ok(DesktopSessionHandle::new(session_id, session, owner_instance))
    }

    fn observe(
        &self,
        handle: &DesktopSessionHandle,
        region: Option<[i32; 4]>,
    ) -> Result<DesktopFrame, DesktopError> {
        let session = self.session(handle.session_id)?;
        let (full_raw, full_rgb, full_width, full_height) = capture_display(session.display_id)?;
        let bounds = unsafe { CGDisplayBounds(session.display_id) };
        let epoch = session
            .domain
            .lock()
            .map(|d| d.lease_state.epoch)
            .unwrap_or(0);
        let (frame_id, revision) = {
            let mut shared = session.locked()?;
            shared.next_frame += 1;
            shared.next_revision += 1;
            (shared.next_frame, shared.next_revision)
        };
        let full_region = [
            bounds.origin.x as i32,
            bounds.origin.y as i32,
            full_width as i32,
            full_height as i32,
        ];
        let region = region.unwrap_or(full_region);
        // Crop the full-display capture to the requested region. Regions
        // outside the display are rejected; partial overlap clips.
        let (raw, rgb, width, height) = crop_to_region(
            full_raw,
            full_rgb,
            full_width,
            full_height,
            region,
            full_region,
        )?;
        Ok(DesktopFrame {
            region,
            width: width as u32,
            height: height as u32,
            raw_pixels: raw,
            rgb,
            pixel_format: DesktopPixelFormat {
                name: "bgra8".into(),
                depth: 32,
                bits_per_pixel: 32,
                byte_order: "little".into(),
                visual_id: 0,
                red_mask: 0x00FF0000,
                green_mask: 0x0000FF00,
                blue_mask: 0x000000FF,
            },
            layout_fingerprint: layout_fingerprint(),
            dpi_x: session.dpi,
            dpi_y: session.dpi,
            scale_factor: session.dpi / 72.0,
            frame_id,
            revision,
            lease_epoch: epoch,
            screen_num: session.display_id as usize,
            detail: serde_json::json!({
                "display_id": session.display_id,
                "accessibility_trusted": session.trusted,
            }),
        })
    }

    fn lease_op(
        &self,
        handle: &DesktopSessionHandle,
        op: DesktopLeaseOp,
        token: Option<EntityId>,
        caller_instance: EntityId,
    ) -> Result<DesktopLeaseOutcome, DesktopError> {
        let session = self.session(handle.session_id)?;
        let mut domain = session.domain_locked()?;
        let (mut outcome, release_inputs) = apply_lease_op(
            &mut domain.lease_state,
            op,
            token,
            caller_instance,
            handle.session_id,
        )?;
        if release_inputs {
            let held: Vec<HeldInput> = domain.held.iter().copied().collect();
            domain.held.clear();
            outcome.held_released = held.len();
            release_held(&held);
        }
        Ok(outcome)
    }

    fn inject(
        &self,
        handle: &DesktopSessionHandle,
        mut request: DesktopInjectRequest,
        operation_id: EntityId,
    ) -> Result<DesktopInjectOutcome, DesktopError> {
        let session = self.session(handle.session_id)?;
        verify_observation_state(&session, &request.observation, request.require_pixel_fresh)?;
        // Every event of this batch is created from a private event source, so
        // the provider's own tap sees them with `kCGEventSourceStatePrivate`
        // and can never mistake this backend's input for a human takeover.
        let source = unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_PRIVATE) };
        let mut receipts = Vec::new();
        let mut sent = 0usize;
        let mut failure: Option<&'static str> = None;
        for (index, action) in request.actions.iter().enumerate() {
            if let Err(reason) = (request.checkpoint)() {
                failure = Some(match reason {
                    DesktopError::Rejected { .. } => "cancelled",
                    _ => "transport_lost",
                });
                break;
            }
            let epoch = session
                .domain
                .lock()
                .map(|d| d.lease_state.epoch)
                .unwrap_or(0);
            let mut domain = match session.lease_guard(request.lease_token, epoch) {
                Ok(domain) => domain,
                Err(_) => {
                    receipts.push(DesktopActionReceipt {
                        index,
                        kind: action.kind().into(),
                        transport: "rejected",
                        observed_effect: Some("lease_revoked"),
                    });
                    failure = Some("lease_revoked");
                    break;
                }
            };
            let mut held_now: Vec<HeldInput> = Vec::new();
            let mut released_now: Vec<HeldInput> = Vec::new();
            let delivered: Result<(), &'static str> = match action {
                DesktopAction::PointerMove { x, y } => {
                    let position = CgPoint {
                        x: f64::from(*x),
                        y: f64::from(*y),
                    };
                    let dragging = !domain.held.is_empty();
                    post_event(|| unsafe {
                        CGEventCreateMouseEvent(
                            source,
                            if dragging {
                                K_CG_EVENT_LEFT_MOUSE_DRAGGED
                            } else {
                                K_CG_EVENT_MOUSE_MOVED
                            },
                            position,
                            K_CG_MOUSE_BUTTON_LEFT,
                        )
                    })
                }
                DesktopAction::Button { button, press } => {
                    let Some(button_const) = mouse_button(*button) else {
                        receipts.push(DesktopActionReceipt {
                            index,
                            kind: action.kind().into(),
                            transport: "rejected",
                            observed_effect: Some("button_unsupported"),
                        });
                        continue;
                    };
                    let position = pointer_position().unwrap_or((0.0, 0.0));
                    let position = CgPoint {
                        x: position.0,
                        y: position.1,
                    };
                    let down = matches!(press, DesktopPress::Down | DesktopPress::Tap);
                    let up = matches!(press, DesktopPress::Up | DesktopPress::Tap);
                    let mut ok = Ok(());
                    if down {
                        let Some(event_type) = mouse_event_type(*button, true, false) else {
                            receipts.push(DesktopActionReceipt {
                                index,
                                kind: action.kind().into(),
                                transport: "rejected",
                                observed_effect: Some("button_unsupported"),
                            });
                            continue;
                        };
                        ok = post_event(|| unsafe {
                            CGEventCreateMouseEvent(source, event_type, position, button_const)
                        });
                        if ok.is_ok() {
                            held_now.push(HeldInput::Button(*button));
                        }
                    }
                    if ok.is_ok() && up {
                        let Some(event_type) = mouse_event_type(*button, false, false) else {
                            receipts.push(DesktopActionReceipt {
                                index,
                                kind: action.kind().into(),
                                transport: "rejected",
                                observed_effect: Some("button_unsupported"),
                            });
                            continue;
                        };
                        ok = post_event(|| unsafe {
                            CGEventCreateMouseEvent(source, event_type, position, button_const)
                        });
                        if ok.is_ok() {
                            released_now.push(HeldInput::Button(*button));
                        }
                    }
                    ok
                }
                DesktopAction::Key { key, press } => {
                    let Some(code) = virtual_key_code(key) else {
                        receipts.push(DesktopActionReceipt {
                            index,
                            kind: action.kind().into(),
                            transport: "rejected",
                            observed_effect: Some("key_unmapped"),
                        });
                        continue;
                    };
                    let mut ok = Ok(());
                    match press.unwrap_or(DesktopPress::Tap) {
                        DesktopPress::Tap => {
                            ok = post_event(|| unsafe {
                                CGEventCreateKeyboardEvent(source, code, true)
                            });
                            if ok.is_ok() {
                                ok = post_event(|| unsafe {
                                    CGEventCreateKeyboardEvent(source, code, false)
                                });
                            }
                        }
                        DesktopPress::Down => {
                            ok = post_event(|| unsafe {
                                CGEventCreateKeyboardEvent(source, code, true)
                            });
                            if ok.is_ok() {
                                held_now.push(HeldInput::Key(code));
                            }
                        }
                        DesktopPress::Up => {
                            ok = post_event(|| unsafe {
                                CGEventCreateKeyboardEvent(source, code, false)
                            });
                            if ok.is_ok() {
                                released_now.push(HeldInput::Key(code));
                            }
                        }
                    }
                    ok
                }
                DesktopAction::Text { text } => {
                    let mut ok = Ok(());
                    for ch in text.chars() {
                        let mut buffer = [0u16; 2];
                        let units = ch.encode_utf16(&mut buffer);
                        ok = post_event(|| unsafe {
                            let event = CGEventCreateKeyboardEvent(
                                source,
                                0,
                                true,
                            );
                            if !event.is_null() {
                                CGEventKeyboardSetUnicodeString(event, units.len(), units.as_ptr());
                            }
                            event
                        });
                        if ok.is_ok() {
                            ok = post_event(|| unsafe {
                                let event = CGEventCreateKeyboardEvent(
                                    source,
                                    0,
                                    false,
                                );
                                if !event.is_null() {
                                    CGEventKeyboardSetUnicodeString(event, units.len(), units.as_ptr());
                                }
                                event
                            });
                        }
                        if ok.is_err() {
                            break;
                        }
                    }
                    ok
                }
            };
            match delivered {
                Ok(()) => {
                    sent += 1;
                    for input in held_now {
                        domain.held.insert(input);
                    }
                    for input in released_now {
                        domain.held.remove(&input);
                    }
                    receipts.push(DesktopActionReceipt {
                        index,
                        kind: action.kind().into(),
                        transport: "acked",
                        observed_effect: None,
                    });
                }
                Err(reason) => {
                    receipts.push(DesktopActionReceipt {
                        index,
                        kind: action.kind().into(),
                        transport: "lost",
                        observed_effect: None,
                    });
                    failure = Some(reason);
                    break;
                }
            }
        }
        let cancelled = failure == Some("cancelled");
        if cancelled {
            if let Ok(mut domain) = session.domain_locked() {
                let held: Vec<HeldInput> = domain.held.iter().copied().collect();
                domain.held.clear();
                release_held(&held);
            }
        }
        if let Ok(mut shared) = session.locked() {
            shared.send_log.insert(operation_id, sent);
        }
        let pointer_post = pointer_position().map(|(x, y)| [x as i32, y as i32]);
        let result = DesktopInjectOutcome {
            receipts,
            sent_events: sent,
            postcondition_verified: false,
            stopped_reason: failure.map(str::to_string),
            detail: serde_json::json!({
                "pointer_post": pointer_post,
                "sent_events": sent,
                "cancelled": cancelled,
                "event_source": if source.is_null() { "null" } else { "private" },
            }),
        };
        if !source.is_null() {
            unsafe { CFRelease(source) };
        }
        Ok(result)
    }

    fn close(
        &self,
        handle: &DesktopSessionHandle,
        caller_instance: EntityId,
    ) -> Result<DesktopCloseOutcome, DesktopError> {
        let session = self.session(handle.session_id)?;
        if session.owner_instance != caller_instance {
            return Err(DesktopError::rejected(
                "not_owner",
                "only the owning controller instance closes this session",
            ));
        }
        session.closed.store(true, Ordering::Relaxed);
        let (held_released, lease_revoked) = if let Ok(mut domain) = session.domain_locked() {
            domain.session_ids.remove(&handle.session_id);
            let holder_gone = domain
                .lease_state
                .holder
                .as_ref()
                .is_some_and(|l| l.holder_session == handle.session_id);
            if domain.session_ids.is_empty() || holder_gone {
                domain.lease_state.holder = None;
                domain.lease_state.epoch += 1;
                let held: Vec<HeldInput> = domain.held.iter().copied().collect();
                domain.held.clear();
                release_held(&held);
                (held.len(), true)
            } else {
                (0, false)
            }
        } else {
            (0, false)
        };
        if let Ok(mut state) = self.locked() {
            state.sessions.remove(&handle.session_id);
        }
        Ok(DesktopCloseOutcome {
            held_released,
            lease_revoked,
        })
    }

    fn reconcile(
        &self,
        handle: &DesktopSessionHandle,
        operation_id: EntityId,
    ) -> DesktopReconcileOutcome {
        let mut sent: Option<usize> = None;
        let mut alive = false;
        if let Ok(session) = self.session(handle.session_id) {
            alive = !session.closed.load(Ordering::Relaxed);
            if let Ok(shared) = session.locked() {
                sent = shared.send_log.get(&operation_id).copied();
            }
        }
        DesktopReconcileOutcome {
            sent_events: sent,
            session_alive: alive,
            process_stop_proven: false,
            detail: serde_json::json!({
                "observation": "controller recovered without a conclusive CGEventPost result",
                "sent_events_before_recovery": sent,
                "session_alive_in_memory": alive,
                "process_stop_proven": false,
            }),
        }
    }
}

fn verify_observation_state(
    session: &MacSession,
    observation: &DesktopObservation,
    require_pixel_fresh: bool,
) -> Result<(), DesktopError> {
    if observation.lease_epoch_at_capture
        != session
            .domain
            .lock()
            .map(|d| d.lease_state.epoch)
            .unwrap_or(0)
    {
        return Err(DesktopError::rejected(
            "lease_epoch_changed",
            "the observation was captured under a different lease epoch",
        ));
    }
    if layout_fingerprint() != observation.layout_fingerprint {
        return Err(DesktopError::rejected(
            "observation_stale_layout",
            "the live display layout no longer matches the bound observation",
        ));
    }
    if require_pixel_fresh {
        let (raw, _, _, _) = capture_display(session.display_id)?;
        if hex_digest(&raw) != observation.pixel_hash {
            return Err(DesktopError::rejected(
                "observation_stale_pixels",
                "the observed pixels changed before input",
            ));
        }
    }
    Ok(())
}
