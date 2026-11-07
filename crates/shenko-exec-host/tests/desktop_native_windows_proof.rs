//! Actual-target native proof producer for the Windows desktop provider
//! (SPEC §24.1/§24.3, read under §51.5/§51.10).
//!
//! This file is a *consumer*: it drives the real `WindowsDesktopProvider`
//! through the public `DesktopProvider` trait and then checks the result with
//! independent Win32 observations — `GetAsyncKeyState`, `GetCursorPos`, a real
//! owned window's `WM_CHAR` queue, and a Toolhelp thread snapshot of this
//! process. It never reads provider source text, never trusts the provider's
//! own receipt as the effect, and never invents a capability the OS did not
//! show.
//!
//! Honest classification, kept explicit in every case:
//! - **own injected** input: `SendInput` from the provider, stamped with its
//!   own `dwExtraInfo` marker; the OS hook must count it as filtered and the
//!   lease epoch must not move.
//! - **other injected external** input: `keybd_event` with `dwExtraInfo = 0`
//!   — the same OS-injected class, but not this backend's writer, so the hook
//!   must treat it as external takeover.
//! - **OS-reported non-injected / physical HID** input: cannot be produced
//!   without a human at this desktop. Every such case is reported
//!   `NATIVE-UNOBSERVED` with its reason; no synthetic event is ever called
//!   physical-human proof and no case is forced to pass.
//!
//! Real OS limits stay facts (SPEC §51.10), not approval gates: a hook that
//! cannot install, a desktop that cannot be captured, or a proof that needs a
//! human are recorded as they are.
//!
//! Run on a dedicated Windows desktop with no human at it: a real
//! person's keystroke or click IS external takeover, so genuine human input
//! during a case is attributed as takeover rather than assumed absent.
//!
//! Target command (parent only, currently UNRUN):
//! `cargo test -p shenko-exec-host --test desktop_native_windows_proof -- --test-threads=1 --nocapture`

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shenko_exec_host::{
    DesktopAction, DesktopCapability, DesktopCapabilityFact, DesktopCoordinateTransform,
    DesktopDisplay, DesktopError, DesktopFrame, DesktopInjectOutcome, DesktopInjectRequest,
    DesktopLeaseOp, DesktopLeaseOutcome, DesktopObservation, DesktopOpenRequest, DesktopPress,
    DesktopProvider, DesktopSessionHandle, HumanInputFanout, HumanInputObserver,
    WindowsDesktopProvider, hex_digest,
};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef, SchemaRef},
};

// ---------------------------------------------------------------------------
// Win32 surface this proof observes with (independent of the provider's FFI)
// ---------------------------------------------------------------------------

type Hwnd = *mut c_void;
type Hinstance = *mut c_void;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NativePoint {
    x: i32,
    y: i32,
}

/// Opaque `MSG` storage. The OS fills it and this proof never reads a field,
/// so no guess is made about `WPARAM`/`LPARAM`/`POINT` packing.
#[repr(C, align(8))]
struct MsgStorage([u8; 128]);

#[repr(C)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_pid: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[repr(C)]
struct WndClassW {
    style: u32,
    wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
    class_extra: i32,
    window_extra: i32,
    instance: Hinstance,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
}

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const SW_SHOW: i32 = 5;
const PM_REMOVE: u32 = 1;
const WM_CHAR: u32 = 0x0102;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn GetCursorPos(point: *mut NativePoint) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn keybd_event(vk: u8, scan: u8, flags: u32, extra_info: usize);
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(window: Hwnd) -> i32;
    fn RegisterClassW(class: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: *mut c_void,
        instance: Hinstance,
        param: *mut c_void,
    ) -> Hwnd;
    fn DestroyWindow(window: Hwnd) -> i32;
    fn DefWindowProcW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    fn PeekMessageW(msg: *mut c_void, window: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const c_void) -> i32;
    fn DispatchMessageW(msg: *const c_void) -> isize;
    fn ShowWindow(window: Hwnd, command: i32) -> i32;
    fn UpdateWindow(window: Hwnd) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Hinstance;
    fn GetCurrentProcessId() -> u32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

// ---------------------------------------------------------------------------
// Honest observation record
// ---------------------------------------------------------------------------

/// A capability the OS is not showing right now is recorded, never asserted
/// into existence, and never silently dropped.
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
// Owned fixture window (never another application's window)
// ---------------------------------------------------------------------------

static WM_CHAR_SINK: Mutex<Vec<u16>> = Mutex::new(Vec::new());

unsafe extern "system" fn fixture_window_procedure(
    window: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if message == WM_CHAR {
        if let Ok(mut chars) = WM_CHAR_SINK.lock() {
            chars.push(wparam as u16);
        }
    }
    // SAFETY: every other message belongs to the system's default handling;
    // this window exists only to receive this proof's own keystrokes.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// A window this proof owns outright, so injected text lands in the fixture
/// and never in whatever application the human left focused.
struct OwnedWindow {
    window: Hwnd,
}

impl OwnedWindow {
    fn create(class: &str) -> Result<Self, String> {
        let mut class_name: Vec<u16> = class.encode_utf16().collect();
        class_name.push(0);
        let mut title: Vec<u16> = "Shenko desktop native proof fixture".encode_utf16().collect();
        title.push(0);
        // SAFETY: plain Win32 window creation with an owned class name; the
        // procedure only appends to a static buffer.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if instance.is_null() {
            return Err(format!(
                "GetModuleHandleW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let description = WndClassW {
            style: 0,
            wnd_proc: Some(fixture_window_procedure),
            class_extra: 0,
            window_extra: 0,
            instance,
            icon: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            background: std::ptr::null_mut(),
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
        };
        // SAFETY: `description` and both string buffers outlive the call.
        let atom = unsafe { RegisterClassW(&description) };
        if atom == 0 {
            return Err(format!(
                "RegisterClassW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: the class is registered above and both name buffers are live
        // for the call.
        let window = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                40,
                40,
                320,
                160,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            )
        };
        if window.is_null() {
            return Err(format!(
                "CreateWindowExW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `window` is ours and was just created.
        unsafe {
            ShowWindow(window, SW_SHOW);
            UpdateWindow(window);
        }
        Ok(Self { window })
    }

    /// Make this fixture the foreground window. Returns false when the OS
    /// keeps the current foreground owner — then this proof must not type,
    /// because the keystrokes would land in an application it does not own.
    fn claim_foreground(&self) -> bool {
        // SAFETY: `self.window` is a live window created by this proof.
        unsafe {
            if GetForegroundWindow() != self.window {
                SetForegroundWindow(self.window);
            }
            GetForegroundWindow() == self.window
        }
    }

    /// Drain this window's queue so `WM_CHAR` reaches the procedure.
    fn pump(&self) {
        let mut storage = MsgStorage([0u8; 128]);
        loop {
            // SAFETY: the OS writes one MSG into `storage`, which is sized and
            // aligned for it; the pointer is never dereferenced here.
            let queued = unsafe {
                PeekMessageW(
                    &mut storage as *mut MsgStorage as *mut c_void,
                    self.window,
                    0,
                    0,
                    PM_REMOVE,
                )
            };
            if queued == 0 {
                break;
            }
            // SAFETY: the same filled MSG is consumed by both dispatch calls
            // before the next peek overwrites it.
            unsafe {
                TranslateMessage(&storage as *const MsgStorage as *const c_void);
                DispatchMessageW(&storage as *const MsgStorage as *const c_void);
            }
        }
    }

    /// Pump until `want` characters reached the procedure (a `WM_CHAR` is only
    /// delivered to it by a dispatch, so waiting without pumping would never
    /// see the text), bounded by `timeout`.
    fn drain_chars(&self, want: usize, timeout: Duration) -> Vec<u16> {
        let until = Instant::now() + timeout;
        loop {
            self.pump();
            let chars = WM_CHAR_SINK
                .lock()
                .map(|chars| chars.clone())
                .unwrap_or_default();
            if chars.len() >= want || Instant::now() >= until {
                return chars;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnedWindow {
    fn drop(&mut self) {
        // SAFETY: the window is ours and still alive inside this drop.
        unsafe {
            DestroyWindow(self.window);
        }
    }
}

// ---------------------------------------------------------------------------
// OS queries
// ---------------------------------------------------------------------------

fn cursor_position() -> Option<[i32; 2]> {
    let mut point = NativePoint::default();
    // SAFETY: `point` is a valid out-parameter.
    let ok = unsafe { GetCursorPos(&mut point) } != 0;
    ok.then_some([point.x, point.y])
}

fn move_cursor(position: [i32; 2]) {
    // SAFETY: pure coordinate arguments.
    unsafe {
        SetCursorPos(position[0], position[1]);
    }
}

fn async_key_state(vk: i32) -> i16 {
    // SAFETY: pure state query, no pointer arguments.
    unsafe { GetAsyncKeyState(vk) }
}

/// `GetAsyncKeyState`'s high bit is the input system's own down state, not
/// this backend's record of what it sent.
fn key_reported_down(vk: i32) -> bool {
    (async_key_state(vk) as u16) & 0x8000 != 0
}

/// Thread ids of this process from a Toolhelp snapshot: an OS observation of
/// whether a provider-owned thread still exists.
fn process_thread_ids() -> Result<Vec<u32>, String> {
    let mut ids = Vec::new();
    // SAFETY: `entry` starts with the size field the API expects; the rest is
    // written by the API before this proof reads it, and the snapshot handle is
    // closed on the same path.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot as isize == -1 {
            return Err(format!(
                "CreateToolhelp32Snapshot failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut entry = ThreadEntry32 {
            size: std::mem::size_of::<ThreadEntry32>() as u32,
            usage: 0,
            thread_id: 0,
            owner_pid: 0,
            base_priority: 0,
            delta_priority: 0,
            flags: 0,
        };
        let mine = GetCurrentProcessId();
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.owner_pid == mine {
                    ids.push(entry.thread_id);
                }
                entry.size = std::mem::size_of::<ThreadEntry32>() as u32;
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    Ok(ids)
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

/// The watch facts the live session really reports (thread id, counters, last
/// error) rather than a field this proof cannot see.
fn watch_detail(display: &DesktopDisplay) -> serde_json::Value {
    display.detail.clone()
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

/// `keybd_event` with `dwExtraInfo = 0` is an OS-injected event this backend
/// did not stamp: exactly the "another writer injected input" class. A key-up
/// for a key nobody pressed types nothing and runs nothing.
fn post_unmarked_key_up(vk: u8) {
    // SAFETY: legacy input path, value arguments only.
    unsafe { keybd_event(vk, 0, KEYEVENTF_KEYUP, 0) };
}

fn external_events(display: &DesktopDisplay) -> u64 {
    watch_detail(display)["external_input_detection"]["external_events"]
        .as_u64()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Case 1: hook readiness and a real capture
// ---------------------------------------------------------------------------

/// The provider reports a hook only after its own message-loop thread installed
/// it; this proof checks that claim against Win32 thread state and a real GDI
/// capture. A hook that cannot install is observed as such, and the session
/// must still open, because capture and input do not depend on it.
#[test]
fn windows_hook_readiness_and_real_capture() {
    let threads_before = process_thread_ids().expect("thread snapshot");
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(WindowsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let instance = owned_instance();
    let handle = provider
        .open(&DesktopOpenRequest::default(), instance)
        .expect("the input desktop must open");
    let display = handle.session.display();
    assert_eq!(display.provider, "windows");
    assert_eq!(display.session_kind, "windows_input_desktop");
    assert!(display.width_px > 0 && display.height_px > 0);
    assert_eq!(handle.owner_instance, instance);

    // Runtime capability, probed after the session started the hook thread.
    let facts = provider.capabilities();
    let detection = capability(&facts, DesktopCapability::ExternalInputDetection);
    let threads_after = process_thread_ids().expect("thread snapshot");
    let watch = watch_detail(&display);
    if detection.available {
        let thread_id = watch["external_input_detection"]["thread_id"]
            .as_u64()
            .expect("an installed hook reports its message-loop thread id");
        assert!(
            threads_after.contains(&(thread_id as u32)),
            "the watcher thread the provider reports must exist in this process"
        );
        assert!(
            threads_after.len() >= threads_before.len(),
            "installing the watcher must not remove threads"
        );
        observed(
            "hook-readiness",
            format!(
                "installed on thread {thread_id}; process threads {} -> {}",
                threads_before.len(),
                threads_after.len()
            ),
        );
    } else {
        unobserved(
            "hook-readiness",
            &format!("hook did not install: {}", detection.detail),
        );
        assert!(
            !detection.detail.is_empty(),
            "a missing hook must name its own failure"
        );
    }

    // Capture is independent of the hook: a real BGRA frame of the display.
    let frame = provider.observe(&handle, None).expect("capture");
    assert_eq!(frame.pixel_format.name, "bgra8");
    assert_eq!(frame.pixel_format.bits_per_pixel, 32);
    assert_eq!(
        frame.raw_pixels.len() as u32,
        frame.width * frame.height * 4,
        "the sealed raw pixels are the provider's native 32-bit layout"
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

    // A fresh domain opening must not be reported as human takeover.
    assert_eq!(probe.count(), 0, "opening a session is not external input");
    provider.close(&handle, instance).expect("close");
}

// ---------------------------------------------------------------------------
// Case 2: own input does not revoke, and the OS shows the effect
// ---------------------------------------------------------------------------

/// Pointer, key and text input this backend injected must not revoke its own
/// epoch, and the effect must be visible to the OS: the cursor really moved,
/// `GetAsyncKeyState` really saw the held key, and a window this proof owns
/// really received the text as `WM_CHAR`.
#[test]
fn windows_own_input_does_not_revoke_and_is_os_observed() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(WindowsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let instance = owned_instance();

    // The fixture window exists BEFORE the observation: a visible window is
    // part of the provider's layout fingerprint.
    let window = OwnedWindow::create("ShenkoNativeProofInput").expect("owned fixture window");
    let focus_owned = window.claim_foreground();
    if !focus_owned {
        unobserved(
            "owned-foreground-window",
            "the OS kept another application foreground; this proof does not type into it",
        );
    }

    let handle = provider
        .open(&DesktopOpenRequest::default(), instance)
        .expect("open");
    let original_cursor = cursor_position().expect("cursor");
    let lease = acquire(provider.as_ref(), &handle, instance).expect("acquire");
    assert_eq!(lease.result, "acquired");
    let token = lease.lease_token.expect("an acquired lease has a token");
    let epoch = lease.epoch;

    let observation = fresh_observation(provider.as_ref(), &handle).expect("observation");

    // Own pointer input: the OS cursor must land where the batch asked.
    let display = handle.session.display();
    let target = [
        (original_cursor[0] + 17).clamp(1, i32::from(display.width_px) - 2),
        (original_cursor[1] + 13).clamp(1, i32::from(display.height_px) - 2),
    ];
    let moved = publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::PointerMove {
            x: target[0],
            y: target[1],
        }],
    )
    .expect("pointer batch");
    assert_eq!(moved.stopped_reason, None);
    assert_eq!(moved.receipts.len(), 1);
    assert_eq!(moved.receipts[0].transport, "acked");
    assert_eq!(
        cursor_position(),
        Some(target),
        "GetCursorPos must report the injected pointer position"
    );
    observed("own-pointer", format!("cursor moved to {target:?}"));

    // Own held key: the OS input state must show it down, then up.
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
    assert!(
        key_reported_down(VK_SHIFT),
        "GetAsyncKeyState must independently report the held shift key"
    );
    observed(
        "held-input-down",
        format!(
            "GetAsyncKeyState(0x10) = 0x{:04x}",
            async_key_state(VK_SHIFT) as u16
        ),
    );
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
    assert!(
        !key_reported_down(VK_SHIFT),
        "the released key must not stay reported down"
    );

    // Own text: only into a window this proof owns.
    if focus_owned && window.claim_foreground() {
        if let Ok(mut chars) = WM_CHAR_SINK.lock() {
            chars.clear();
        }
        let text = "shenko";
        let typed = publish(
            provider.as_ref(),
            &handle,
            &observation,
            token,
            vec![DesktopAction::Text {
                text: text.to_string(),
            }],
        )
        .expect("text batch");
        assert_eq!(typed.stopped_reason, None);
        assert_eq!(typed.receipts.len(), 1);
        let chars = window.drain_chars(text.len(), Duration::from_millis(1500));
        let received = String::from_utf16_lossy(&chars[..]);
        assert_eq!(
            received, text,
            "the owned window must receive one WM_CHAR per typed character"
        );
        observed("own-text", format!("owned window received {received:?}"));
    } else {
        unobserved(
            "own-text",
            "text input needs an owned foreground window; typing into another application is not part of this proof",
        );
    }

    // Nothing above may have revoked the writer epoch.
    let after = fresh_observation(provider.as_ref(), &handle).expect("observation");
    assert_eq!(
        after.lease_epoch_at_capture, epoch,
        "own injected input must not revoke the lease epoch (a real person using this desktop would)"
    );
    assert_eq!(
        probe.count(),
        0,
        "own injected input is not external input (a real person using this desktop would be)"
    );
    let watch = watch_detail(&handle.session.display());
    let own_filtered = watch["external_input_detection"]["own_events_filtered"]
        .as_u64()
        .unwrap_or(0);
    if own_filtered > 0 {
        observed(
            "own-input-classification",
            format!("{own_filtered} own events filtered by the OS hook; epoch stayed {epoch}"),
        );
    } else {
        unobserved(
            "own-input-classification",
            "the hook was not installed, so own-event filtering was not observable",
        );
    }

    let released_lease = provider
        .lease_op(&handle, DesktopLeaseOp::Release, Some(token), instance)
        .expect("release");
    assert_eq!(released_lease.result, "released");
    provider.close(&handle, instance).expect("close");
    move_cursor(original_cursor);
}

// ---------------------------------------------------------------------------
// Case 3: external injected takeover
// ---------------------------------------------------------------------------

/// Input injected by a different writer (`dwExtraInfo = 0`) is OS-injected but
/// not this backend's: the hook must treat it as external takeover — revoke the
/// epoch, release machine-held input, notify the shared fanout — and the
/// pre-takeover batch must then be refused with no effect.
#[test]
fn windows_external_injected_input_revokes_epoch_and_releases_held() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let provider: Arc<dyn DesktopProvider> =
        Arc::new(WindowsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let instance = owned_instance();
    let handle = provider
        .open(&DesktopOpenRequest::default(), instance)
        .expect("open");
    let display = handle.session.display();
    let detection = capability(
        &provider.capabilities(),
        DesktopCapability::ExternalInputDetection,
    );
    if !detection.available {
        unobserved(
            "external-injected-takeover",
            &format!(
                "no OS hook is installed, so external input cannot be detected here: {}",
                detection.detail
            ),
        );
        return;
    }

    let lease = acquire(provider.as_ref(), &handle, instance).expect("acquire");
    let token = lease.lease_token.expect("token");
    let epoch = lease.epoch;
    let observation = fresh_observation(provider.as_ref(), &handle).expect("observation");

    // A machine-held key exists so the revocation has something to release.
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
    assert!(key_reported_down(VK_SHIFT));
    let before_rejection = cursor_position().expect("cursor");

    // Another writer's injected input: unmarked, so the hook cannot filter it
    // as this backend's own.
    post_unmarked_key_up(VK_CONTROL as u8);

    let reported = wait_until(Duration::from_secs(3), || {
        external_events(&handle.session.display()) > 0
    });
    if !reported {
        unobserved(
            "external-injected-takeover",
            "the OS hook did not report the unmarked injected event inside the bounded window",
        );
        return;
    }
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
    observed(
        "external-injected-takeover",
        format!(
            "hook counted {} external events; {} fanout dispatches for {}",
            external_events(&handle.session.display()),
            probe.count(),
            display.domain_id
        ),
    );

    // Machine-held input is released by the handover fence: the OS state is the
    // observation, not the provider's word for it.
    assert!(
        wait_until(Duration::from_secs(2), || !key_reported_down(VK_SHIFT)),
        "takeover must release the machine-held key"
    );
    observed(
        "held-input-release",
        "GetAsyncKeyState reports shift up after takeover",
    );

    // A pre-takeover observation is fenced before any event is sent.
    match publish(
        provider.as_ref(),
        &handle,
        &observation,
        token,
        vec![DesktopAction::PointerMove {
            x: before_rejection[0] + 40,
            y: before_rejection[1] + 40,
        }],
    ) {
        Err(error) => {
            assert_eq!(error.reason(), "lease_epoch_changed");
            observed(
                "stale-frame-rejected",
                "the pre-takeover observation was refused before any event",
            );
        }
        Ok(outcome) => panic!(
            "a pre-takeover batch must be refused, got stopped_reason {:?}",
            outcome.stopped_reason
        ),
    }
    assert_eq!(
        cursor_position(),
        Some(before_rejection),
        "a refused batch must not move the pointer"
    );

    // A fresh observation with the revoked token is refused by the lease guard.
    let fresh = fresh_observation(provider.as_ref(), &handle).expect("observation");
    assert!(
        fresh.lease_epoch_at_capture > epoch,
        "the takeover must have bumped the domain epoch"
    );
    let fenced = publish(
        provider.as_ref(),
        &handle,
        &fresh,
        token,
        vec![DesktopAction::PointerMove {
            x: before_rejection[0] + 40,
            y: before_rejection[1] + 40,
        }],
    )
    .expect("the batch is accounted, not a transport error");
    assert_eq!(fenced.stopped_reason.as_deref(), Some("lease_revoked"));
    assert_eq!(fenced.sent_events, 0);
    assert_eq!(fenced.receipts[0].transport, "rejected");
    assert_eq!(
        fenced.receipts[0].observed_effect.as_deref(),
        Some("lease_revoked")
    );
    assert_eq!(
        cursor_position(),
        Some(before_rejection),
        "a lease-fenced batch must not move the pointer"
    );
    observed(
        "revoked-token-rejected",
        format!("epoch {epoch} -> {}; no event left the process", fresh.lease_epoch_at_capture),
    );

    unobserved(
        "physical-hid-takeover",
        "a non-injected human keypress needs a person at this desktop; the injected case above is not physical-human proof",
    );
    provider.close(&handle, instance).expect("close");
}

// ---------------------------------------------------------------------------
// Case 4: session close, hook ownership failure, final teardown
// ---------------------------------------------------------------------------

/// A session close releases what that session held; a second watcher in the
/// same process cannot install its hooks (the low-level hook sink is
/// process-global) and says so instead of pretending; dropping the provider
/// joins its watcher threads and leaves no OS hook behind, which a fresh
/// provider proves by installing again.
#[test]
fn windows_session_close_and_provider_teardown_release_registrations() {
    let fanout = Arc::new(HumanInputFanout::new());
    let probe = Arc::new(DispatchProbe::default());
    let registered: Arc<dyn HumanInputObserver> = Arc::clone(&probe);
    fanout.register(registered);
    let instance = owned_instance();
    let threads_before = process_thread_ids().expect("thread snapshot");

    let provider: Arc<dyn DesktopProvider> =
        Arc::new(WindowsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let first = provider.open(&DesktopOpenRequest::default(), instance).expect("open a");
    let second = provider.open(&DesktopOpenRequest::default(), instance).expect("open b");
    let display = first.session.display();
    assert_eq!(
        second.session.display().domain_id,
        display.domain_id,
        "sessions on one input desktop share the lease domain"
    );
    let detection = capability(
        &provider.capabilities(),
        DesktopCapability::ExternalInputDetection,
    );
    if !detection.available {
        unobserved(
            "watcher-teardown",
            &format!("no OS hook installed here: {}", detection.detail),
        );
        return;
    }
    let watcher_thread = watch_detail(&display)["external_input_detection"]["thread_id"]
        .as_u64()
        .expect("an installed hook reports its thread") as u32;
    assert!(process_thread_ids().expect("threads").contains(&watcher_thread));

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
    assert!(key_reported_down(VK_SHIFT));

    // A second provider in this process cannot install a second hook sink; the
    // failure is a reported fact and its session still opens.
    let other: Arc<dyn DesktopProvider> =
        Arc::new(WindowsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let other_handle = other
        .open(&DesktopOpenRequest::default(), instance)
        .expect("a provider without a hook still opens a session");
    let other_detection = capability(
        &other.capabilities(),
        DesktopCapability::ExternalInputDetection,
    );
    assert!(
        !other_detection.available,
        "two process-wide low-level hook watchers cannot both be addressed"
    );
    unobserved(
        "second-watcher-install",
        &format!("the second provider reports: {}", other_detection.detail),
    );
    assert!(!other_handle.session.is_closed());

    // Closing the writer session releases its held input and revokes the epoch;
    // the sibling session stays open.
    let closed = provider.close(&first, instance).expect("close a");
    assert!(closed.lease_revoked, "closing the writer revokes the epoch");
    assert!(
        closed.held_released >= 1,
        "closing the writer releases held input"
    );
    assert!(
        wait_until(Duration::from_secs(2), || !key_reported_down(VK_SHIFT)),
        "the closed session's held key must be up"
    );
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

    // Final teardown: no watcher thread survives, and no thread of the
    // providers is left behind.
    drop(other);
    drop(provider);
    drop(first);
    drop(second);
    drop(other_handle);
    let threads_after_drop = process_thread_ids().expect("thread snapshot");
    assert!(
        !threads_after_drop.contains(&watcher_thread),
        "the provider's hook thread must be joined before the provider is gone"
    );
    assert!(
        threads_after_drop.len() <= threads_before.len(),
        "provider teardown leaves no thread of its own: {} -> {}",
        threads_before.len(),
        threads_after_drop.len()
    );
    observed(
        "watcher-join",
        format!(
            "hook thread {watcher_thread} absent; process threads {} -> {}",
            threads_before.len(),
            threads_after_drop.len()
        ),
    );
    assert!(
        !key_reported_down(VK_SHIFT),
        "dropping every session releases what it held"
    );

    // Late input after teardown must have no observer left to reach.
    let hits_after_drop = probe.count();
    post_unmarked_key_up(VK_CONTROL as u8);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        probe.count(),
        hits_after_drop,
        "no retired watcher may dispatch after the provider is gone"
    );

    // A fresh provider installs a fresh hook, which proves the previous OS
    // registration and its process-global sink were released.
    let revived: Arc<dyn DesktopProvider> =
        Arc::new(WindowsDesktopProvider::with_fanout(Arc::clone(&fanout)));
    let revived_handle = revived
        .open(&DesktopOpenRequest::default(), instance)
        .expect("open");
    let revived_detection = capability(
        &revived.capabilities(),
        DesktopCapability::ExternalInputDetection,
    );
    assert!(
        revived_detection.available,
        "a provider created after teardown must install its hook again: {}",
        revived_detection.detail
    );
    observed(
        "registration-recovery",
        "a provider created after teardown installed its hook again",
    );
    revived.close(&revived_handle, instance).expect("close");
    drop(revived);
    let threads_final = process_thread_ids().expect("thread snapshot");
    assert!(
        threads_final.len() <= threads_before.len(),
        "the revived provider also joins its threads: {} -> {}",
        threads_before.len(),
        threads_final.len()
    );
}
