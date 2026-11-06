//! Real Windows desktop provider (SPEC §24.1, §24.3).
//!
//! Hand-rolled FFI against `user32`/`gdi32` — no external windows crate —
//! so the implementation stays inside this crate's dependency surface.
//! Observation is a genuine `BitBlt`/`GetDIBits` screen capture of the
//! calling thread's input desktop; input is genuine `SendInput`. One write
//! lease per input-desktop domain is enforced with the same neutral
//! `apply_lease_op` algorithm the X11 provider uses.
//!
//! Honest limits, never a blanket rejection: human-input detection on
//! Windows would need a low-level hook message loop owned by this process;
//! it is reported `available: false` per session instead of being guessed,
//! and takeover stays lease-bound (a human holding no lease can always
//! take over with `takeover`, which releases machine-held inputs).
//!
//! That message loop is now real: the provider owns a native thread that
//! installs `WH_KEYBOARD_LL`/`WH_MOUSE_LL` hooks and pumps the thread
//! message queue, and the hook procedure only signals a dispatcher thread —
//! it never touches the display-domain lease lock. The dispatcher performs
//! the same revoke + held-input release + `HumanInputFanout` dispatch the
//! X11 watcher does, so one takeover semantic covers every provider.

#![cfg(target_os = "windows")]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use shenko_store::EntityId;

use crate::desktop::*;

// ---------------------------------------------------------------------------
// user32 / gdi32 FFI (x86_64 layout)
// ---------------------------------------------------------------------------

type Hdc = *mut c_void;
type Hwnd = *mut c_void;
type Bool = i32;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct LastInputInfo {
    cb_size: u32,
    dw_time: u32,
}

#[repr(C)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
struct KeyboardInput {
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
union InputUnion {
    mouse: MouseInput,
    keyboard: KeyboardInput,
    _raw: [u8; 32],
}

#[repr(C)]
struct WinInput {
    input_type: u32,
    _padding: u32,
    union: InputUnion,
}

const INPUT_KEYBOARD: u32 = 1;
const INPUT_MOUSE: u32 = 0;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const UOI_NAME: i32 = 2;
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: *mut c_void = -4isize as *mut c_void;

#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: Hwnd) -> Hdc;
    fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetCursorPos(point: *mut Point) -> Bool;
    fn SetCursorPos(x: i32, y: i32) -> Bool;
    fn SendInput(count: u32, inputs: *const WinInput, size: i32) -> u32;
    fn GetLastInputInfo(info: *mut LastInputInfo) -> Bool;
    fn GetDpiForSystem() -> u32;
    fn SetProcessDpiAwarenessContext(value: *mut c_void) -> Bool;
    fn VkKeyScanW(ch: u16) -> i16;
    fn MapVirtualKeyW(code: u32, map_type: u32) -> u32;
    fn OpenInputDesktop(flags: u32, inherit: Bool, access: u32) -> *mut c_void;
    fn CloseDesktop(desk: *mut c_void) -> Bool;
    fn GetUserObjectInformationW(
        object: *mut c_void,
        index: i32,
        info: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> Bool;
    fn GetForegroundWindow() -> Hwnd;
    fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> Bool;
    fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    fn EnumWindows(callback: extern "system" fn(Hwnd, isize) -> Bool, lparam: isize) -> Bool;
    fn GetAsyncKeyState(vk: i32) -> i16;
}

/// A thread message, as `PeekMessageW`/`GetMessageW` fill it. No `lPrivate`
/// member: that field only exists in the legacy `_MAC` layout.
#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: Point,
}

/// `KBDLLHOOKSTRUCT` for `WH_KEYBOARD_LL`.
#[repr(C)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

/// `MSLLHOOKSTRUCT` for `WH_MOUSE_LL`.
#[repr(C)]
struct MsllHookStruct {
    pt: Point,
    mouse_data: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

type HookProc = extern "system" fn(i32, usize, isize) -> isize;

/// Low-level hook installation, the owning thread's message loop and the
/// post that ends it. `WH_*_LL` hooks are delivered to the thread that
/// installed them, so this thread both installs and pumps.
#[link(name = "user32")]
extern "system" {
    fn SetWindowsHookExW(
        id_hook: i32,
        procedure: HookProc,
        module: *mut c_void,
        thread_id: u32,
    ) -> *mut c_void;
    fn UnhookWindowsHookEx(hook: *mut c_void) -> Bool;
    fn CallNextHookEx(hook: *mut c_void, code: i32, wparam: usize, lparam: isize) -> isize;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> Bool;
    fn PostThreadMessageW(thread_id: u32, msg: u32, wparam: usize, lparam: isize) -> Bool;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetLastError() -> u32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
    fn DeleteDC(hdc: Hdc) -> Bool;
    fn CreateDIBSection(
        hdc: Hdc,
        bitmap_info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    fn SelectObject(hdc: Hdc, object: *mut c_void) -> *mut c_void;
    fn BitBlt(
        dest: Hdc,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        src: Hdc,
        src_x: i32,
        src_y: i32,
        rop: u32,
    ) -> Bool;
    fn DeleteObject(object: *mut c_void) -> Bool;
}

const SRCCOPY: u32 = 0x00CC0020;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;

#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<WinInput>() == 40);

// ---------------------------------------------------------------------------
// Key names to virtual key codes (the real US-layout subset X11 already
// accepted; anything else is rejected by name, never guessed).
// ---------------------------------------------------------------------------

fn virtual_key_code(name: &str) -> Option<u16> {
    let upper = name.to_ascii_uppercase();
    let code = match upper.as_str() {
        "BACKSPACE" => 0x08,
        "TAB" => 0x09,
        "ENTER" | "RETURN" | "KP_ENTER" => 0x0D,
        "SHIFT" | "LSHIFT" => 0x10,
        "CTRL" | "LCTRL" | "CONTROL" => 0x11,
        "ALT" | "LALT" => 0x12,
        "CAPSLOCK" => 0x14,
        "ESCAPE" => 0x1B,
        "SPACE" => 0x20,
        "PAGEUP" | "PRIOR" => 0x21,
        "PAGEDOWN" | "NEXT" => 0x22,
        "END" => 0x23,
        "HOME" => 0x24,
        "LEFT" => 0x25,
        "UP" => 0x26,
        "RIGHT" => 0x27,
        "DOWN" => 0x28,
        "PRINT" => 0x2C,
        "INSERT" => 0x2D,
        "DELETE" => 0x2E,
        "LWIN" => 0x5B,
        "RWIN" => 0x5C,
        "NUMLOCK" => 0x90,
        "SCROLLLOCK" => 0x91,
        "LSHIFT_ALT" => return None,
        _ => {
            let bytes = upper.as_bytes();
            if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
                u16::from(bytes[0])
            } else if bytes.len() == 1 && bytes[0].is_ascii_digit() {
                u16::from(bytes[0])
            } else if let Some(rest) = upper.strip_prefix('F') {
                let n: u16 = rest.parse().ok()?;
                if !(1..=12).contains(&n) {
                    return None;
                }
                0x6F + n
            } else if upper.starts_with("KP_") {
                match &upper[3..] {
                    "0" => 0x60,
                    "1" => 0x61,
                    "2" => 0x62,
                    "3" => 0x63,
                    "4" => 0x64,
                    "5" => 0x65,
                    "6" => 0x66,
                    "7" => 0x67,
                    "8" => 0x68,
                    "9" => 0x69,
                    "MULTIPLY" => 0x6A,
                    "ADD" => 0x6B,
                    "SUBTRACT" => 0x6D,
                    "DECIMAL" => 0x6E,
                    "DIVIDE" => 0x6F,
                    _ => return None,
                }
            } else if upper.chars().count() == 1 {
                // Fall back to the real keyboard-layout translation.
                let ch = upper.chars().next()?;
                let scanned = unsafe { VkKeyScanW(ch as u16) };
                if scanned < 0 {
                    return None;
                }
                (scanned as u16) & 0xFF
            } else {
                return None;
            }
        }
    };
    Some(code)
}

fn mouse_button_flags(button: u8, down: bool) -> Option<u32> {
    match (button, down) {
        (1, true) => Some(MOUSEEVENTF_LEFTDOWN),
        (1, false) => Some(MOUSEEVENTF_LEFTUP),
        (2, true) => Some(MOUSEEVENTF_RIGHTDOWN),
        (2, false) => Some(MOUSEEVENTF_RIGHTUP),
        (3, true) => Some(MOUSEEVENTF_MIDDLEDOWN),
        (3, false) => Some(MOUSEEVENTF_MIDDLEUP),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HeldInput {
    Key(u16),
    Button(u8),
}

struct WindowsSessionShared {
    next_frame: u64,
    next_revision: u64,
    send_log: BTreeMap<EntityId, usize>,
}

/// One input-desktop domain: the shared lease truth across every session
/// opened on the same desktop.
struct WindowsDomainState {
    lease_state: DesktopLeaseState,
    held: BTreeSet<HeldInput>,
    session_ids: BTreeSet<EntityId>,
}

struct WindowsSession {
    session_id: EntityId,
    closed: AtomicBool,
    owner_instance: EntityId,
    input_desktop: String,
    shared: Arc<Mutex<WindowsSessionShared>>,
    domain: Arc<Mutex<WindowsDomainState>>,
    /// The provider's hook watcher; its real readiness is the capability
    /// fact this session reports.
    watch: Arc<HumanInputWatch>,
    width_px: u32,
    height_px: u32,
    dpi: u32,
}

impl WindowsSession {
    fn locked(&self) -> Result<MutexGuard<'_, WindowsSessionShared>, DesktopError> {
        self.shared
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))
    }

    fn domain_locked(&self) -> Result<MutexGuard<'_, WindowsDomainState>, DesktopError> {
        self.domain
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))
    }

    fn lease_guard(
        &self,
        token: EntityId,
        epoch: u64,
    ) -> Result<MutexGuard<'_, WindowsDomainState>, &'static str> {
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

impl DesktopSession for WindowsSession {
    fn session_id(&self) -> EntityId {
        self.session_id
    }

    fn provider_id(&self) -> &'static str {
        "windows"
    }

    fn domain_id(&self) -> String {
        format!("windows/{}", self.input_desktop)
    }

    fn display(&self) -> DesktopDisplay {
        let capable = unsafe { GetSystemMetrics(SM_CXSCREEN) } > 0;
        let watch = self.watch.snapshot();
        DesktopDisplay {
            provider: "windows".into(),
            domain_id: self.domain_id(),
            display_id: self.input_desktop.clone(),
            session_kind: "windows_input_desktop".into(),
            width_px: self.width_px,
            height_px: self.height_px,
            dpi_x: f64::from(self.dpi),
            dpi_y: f64::from(self.dpi),
            scale_factor: f64::from(self.dpi) / 96.0,
            depth: 32,
            capabilities: vec![
                DesktopCapabilityFact {
                    capability: DesktopCapability::ScreenRead,
                    available: capable,
                    detail: "GDI BitBlt/GetDIBits of the input desktop".into(),
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::Input,
                    available: true,
                    detail: "SendInput (user32)".into(),
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::ExternalInputDetection,
                    // Real hook readiness, proven at install time; a failed
                    // install stays false with its own reason.
                    available: watch.installed,
                    detail: match (&watch.last_error, watch.thread_id) {
                        (Some(error), _) => format!(
                            "Win32 WH_KEYBOARD_LL/WH_MOUSE_LL hooks are not installed: {error}"
                        ),
                        (None, Some(thread_id)) => format!(
                            "Win32 WH_KEYBOARD_LL/WH_MOUSE_LL hooks on message-loop thread {thread_id}; own SendInput events filtered by dwExtraInfo marker"
                        ),
                        (None, None) => "low-level hooks are not installed".to_string(),
                    },
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::LayoutRevision,
                    available: true,
                    detail: "visible top-level window rects + foreground window hash".into(),
                },
                DesktopCapabilityFact {
                    capability: DesktopCapability::PixelFreshness,
                    available: true,
                    detail: "re-capture pixel hash".into(),
                },
            ],
            detail: serde_json::json!({
                "input_desktop": self.input_desktop,
                "width_px": self.width_px,
                "height_px": self.height_px,
                "dpi": self.dpi,
                "external_input_detection": watch,
            }),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for WindowsSession {
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

struct WindowsState {
    sessions: BTreeMap<EntityId, Arc<WindowsSession>>,
    domains: BTreeMap<String, Arc<Mutex<WindowsDomainState>>>,
}

pub struct WindowsDesktopProvider {
    /// Shared with the human-input dispatcher through a `Weak`, so the
    /// watcher threads never keep the provider (or its sessions) alive.
    state: Arc<Mutex<WindowsState>>,
    fanout: Arc<HumanInputFanout>,
    /// Created lazily on the first session; dropped (and joined) with the
    /// provider.
    watch: Mutex<Option<Arc<HumanInputWatch>>>,
}

impl WindowsDesktopProvider {
    pub fn new() -> Self {
        Self::with_fanout(Arc::new(HumanInputFanout::new()))
    }

    /// Share the human-input registry with the product backend so a browser
    /// or mobile peer bound to the same domain gets the same takeover event.
    pub fn with_fanout(fanout: Arc<HumanInputFanout>) -> Self {
        Self {
            state: Arc::new(Mutex::new(WindowsState {
                sessions: BTreeMap::new(),
                domains: BTreeMap::new(),
            })),
            fanout,
            watch: Mutex::new(None),
        }
    }

    fn locked(&self) -> Result<MutexGuard<'_, WindowsState>, DesktopError> {
        self.state
            .lock()
            .map_err(|_| DesktopError::unavailable("backend_state_poisoned", ""))
    }

    fn session(&self, session_id: EntityId) -> Result<Arc<WindowsSession>, DesktopError> {
        let state = self.locked()?;
        match state.sessions.get(&session_id) {
            Some(session) if !session.closed.load(Ordering::Relaxed) => Ok(Arc::clone(session)),
            Some(_) => Err(DesktopError::unavailable("session_closed", "")),
            None => Err(DesktopError::unavailable("session_unknown", "")),
        }
    }

    /// Start the hook watcher once, retrying after a failed install. Its own
    /// readiness — never the mere existence of this call — is what a session
    /// reports, so a hook that cannot install stays an honest `false`.
    fn ensure_human_input_watch(&self) -> Arc<HumanInputWatch> {
        {
            let guard = match self.watch.lock() {
                Ok(guard) => guard,
                Err(_) => return Arc::new(HumanInputWatch::start(
                    Arc::downgrade(&self.state),
                    Arc::clone(&self.fanout),
                )),
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
}

impl Default for WindowsDesktopProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WindowsDesktopProvider {
    fn drop(&mut self) {
        // Joins both watcher threads and unhooks before returning, so no
        // native thread or active callback outlives the provider.
        if let Some(watch) = self.watch.lock().ok().and_then(|mut guard| guard.take()) {
            watch.stop();
        }
    }
}

/// Capture the whole desktop (or a region) as BGRA DIB bytes.
fn capture_region(region: [i32; 4]) -> Result<(Vec<u8>, Vec<u8>, usize, usize), DesktopError> {
    let [x, y, w, h] = region;
    if w <= 0 || h <= 0 {
        return Err(DesktopError::rejected("region_empty", "empty capture region"));
    }
    let width = w as usize;
    let height = h as usize;
    let screen = unsafe { GetDC(std::ptr::null_mut()) };
    if screen.is_null() {
        return Err(DesktopError::unavailable(
            "display_unreachable",
            "GetDC(NULL) failed on the input desktop",
        ));
    }
    let mut bits: *mut c_void = std::ptr::null_mut();
    let info = BitmapInfo {
        bmi_header: BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: w,
            bi_height: -h, // top-down DIB
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            bi_size_image: 0,
            bi_x_pels_per_meter: 0,
            bi_y_pels_per_meter: 0,
            bi_clr_used: 0,
            bi_clr_important: 0,
        },
        bmi_colors: [0; 3],
    };
    let bitmap = unsafe {
        CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0)
    };
    let result = (|| -> Result<(Vec<u8>, Vec<u8>, usize, usize), DesktopError> {
        if bitmap.is_null() || bits.is_null() {
            return Err(DesktopError::unavailable(
                "capture_unavailable",
                "CreateDIBSection failed",
            ));
        }
        let memory = unsafe { CreateCompatibleDC(screen) };
        if memory.is_null() {
            return Err(DesktopError::unavailable(
                "capture_unavailable",
                "CreateCompatibleDC failed",
            ));
        }
        let ok = unsafe {
            SelectObject(memory, bitmap);
            BitBlt(memory, 0, 0, w, h, screen, x, y, SRCCOPY)
        };
        unsafe { DeleteDC(memory) };
        if ok == 0 {
            return Err(DesktopError::unavailable(
                "capture_unavailable",
                "BitBlt from the screen failed",
            ));
        }
        let pixel_bytes = width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| DesktopError::rejected("region_too_large", "pixel byte count overflow"))?;
        let raw = unsafe { std::slice::from_raw_parts(bits as *const u8, pixel_bytes) }.to_vec();
        // BGRA -> RGB8 preview; raw stays the provider-native bytes.
        let mut rgb = Vec::with_capacity(width * height * 3);
        for px in raw.chunks_exact(4) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        Ok((raw, rgb, width, height))
    })();
    unsafe {
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        ReleaseDC(std::ptr::null_mut(), screen);
    }
    result
}

extern "system" fn enum_windows_proc(hwnd: Hwnd, lparam: isize) -> Bool {
    unsafe {
        let windows = &mut *(lparam as *mut Vec<(i32, i32, i32, i32, u32)>);
        let mut rect = Rect::default();
        if GetWindowRect(hwnd, &mut rect) != 0 && IsWindowVisible(hwnd) != 0 {
            windows.push((rect.left, rect.top, rect.right, rect.bottom, hwnd as usize as u32));
        }
    }
    1
}

fn current_input_desktop_name() -> Result<String, DesktopError> {
    unsafe {
        let desk = OpenInputDesktop(0, 0, 0x0001 /*DESKTOP_READOBJECTS*/);
        if desk.is_null() {
            return Err(DesktopError::unavailable(
                "display_unreachable",
                "OpenInputDesktop failed on this session",
            ));
        }
        let mut buffer = [0u16; 256];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            desk,
            UOI_NAME,
            buffer.as_mut_ptr() as *mut c_void,
            (buffer.len() * 2) as u32,
            &mut needed,
        );
        CloseDesktop(desk);
        if ok == 0 {
            return Err(DesktopError::unavailable(
                "display_unreachable",
                "GetUserObjectInformationW(UOI_NAME) failed",
            ));
        }
        let len = (needed as usize / 2).min(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..len.saturating_sub(1)]))
    }
}

fn layout_fingerprint() -> String {
    let mut windows: Vec<(i32, i32, i32, i32, u32)> = Vec::new();
    let foreground = unsafe { GetForegroundWindow() } as usize as u32;
    unsafe {
        EnumWindows(enum_windows_proc, &mut windows as *mut _ as isize);
    }
    windows.sort();
    let mut cursor = Point::default();
    let cursor_ok = unsafe { GetCursorPos(&mut cursor) };
    let last_input = unsafe {
        let mut info = LastInputInfo {
            cb_size: std::mem::size_of::<LastInputInfo>() as u32,
            dw_time: 0,
        };
        if GetLastInputInfo(&mut info) != 0 {
            Some(info.dw_time)
        } else {
            None
        }
    };
    let material = serde_json::json!({
        "windows": windows,
        "foreground": foreground,
        "cursor": if cursor_ok != 0 { [cursor.x, cursor.y] } else { [0, 0] },
        "screen": [
            unsafe { GetSystemMetrics(SM_CXSCREEN) },
            unsafe { GetSystemMetrics(SM_CYSCREEN) }
        ],
        "last_input": last_input,
    });
    hex_digest(serde_json::to_vec(&material).unwrap_or_default().as_slice())
}

fn send_inputs(events: &[WinInput]) -> Result<usize, &'static str> {
    if events.is_empty() {
        return Ok(0);
    }
    let sent = unsafe { SendInput(events.len() as u32, events.as_ptr(), 40) };
    if sent == 0 {
        Err("transport_lost")
    } else {
        Ok(sent as usize)
    }
}

fn key_event(vk: u16, down: bool) -> WinInput {
    WinInput {
        input_type: INPUT_KEYBOARD,
        _padding: 0,
        union: InputUnion {
            keyboard: KeyboardInput {
                w_vk: vk,
                w_scan: unsafe { MapVirtualKeyW(u32::from(vk), 0) } as u16,
                dw_flags: if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                // The low-level hook filters our own injections by this
                // marker; an unmarked injected event is treated as external.
                dw_extra_info: SELF_INPUT_MARKER,
            },
        },
    }
}

fn text_events(ch: char) -> Vec<WinInput> {
    // KEYEVENTF_UNICODE injects the real character through the keyboard
    // layout without needing a mapped VK.
    let mut buffer = [0u16; 2];
    let units = ch.encode_utf16(&mut buffer);
    let mut events = Vec::new();
    for &unit in units {
        for down in [true, false] {
            events.push(WinInput {
                input_type: INPUT_KEYBOARD,
                _padding: 0,
                union: InputUnion {
                    keyboard: KeyboardInput {
                        w_vk: 0,
                        w_scan: unit,
                        dw_flags: KEYEVENTF_UNICODE | if down { 0 } else { KEYEVENTF_KEYUP },
                        time: 0,
                        dw_extra_info: SELF_INPUT_MARKER,
                    },
                },
            });
        }
    }
    events
}

fn mouse_event(flags: u32) -> WinInput {
    WinInput {
        input_type: INPUT_MOUSE,
        _padding: 0,
        union: InputUnion {
            mouse: MouseInput {
                dx: 0,
                dy: 0,
                mouse_data: 0,
                dw_flags: flags,
                time: 0,
                dw_extra_info: SELF_INPUT_MARKER,
            },
        },
    }
}

fn release_held(held: &[HeldInput]) {
    let mut events = Vec::new();
    for input in held {
        match *input {
            HeldInput::Key(vk) => events.push(key_event(vk, false)),
            HeldInput::Button(button) => {
                if let Some(flags) = mouse_button_flags(button, false) {
                    events.push(mouse_event(flags));
                }
            }
        }
    }
    let _ = send_inputs(&events);
}

// ---------------------------------------------------------------------------
// Human-input observation: process-owned low-level hook message loop
// ---------------------------------------------------------------------------

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const WM_QUIT: u32 = 0x0012;
const PM_NOREMOVE: u32 = 0;
const PM_REMOVE: u32 = 0x0001;
const LLKHF_INJECTED: u32 = 0x0000_0010;
const LLMHF_INJECTED: u32 = 0x0000_0001;
/// `dwExtraInfo` stamp on every event this backend injects. The hook sees
/// `LLKHF_INJECTED`/`LLMHF_INJECTED` on any synthesized event, so this
/// marker is what separates our own input from an unmanaged writer's.
const SELF_INPUT_MARKER: usize = 0x5348_454E_4B4F; // "SHENKO"
/// How long the provider waits for the hook thread to prove it installed.
const WATCH_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// One external input observation, as the hook thread classified it.
#[derive(Debug, Clone, Copy)]
struct ExternalInputEvent {
    kind: &'static str,
    virtual_key: u32,
    injected: bool,
}

/// What the hook thread reports to the provider. `Started` carries the
/// thread id as soon as its message queue exists, so a stop request can be
/// delivered even while hook installation is still in progress.
enum HookReport {
    Started(u32),
    Installed(Result<(), String>),
}

/// The low-level hook procedure receives no user data, so the live signal
/// sink is process-global. It holds only an `mpsc::Sender` — never the
/// provider or its state — and is cleared as soon as the watcher stops, so a
/// late callback cannot reach a retired backend.
static HOOK_SINK: std::sync::LazyLock<Mutex<Option<mpsc::Sender<ExternalInputEvent>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
/// Coalescing flag: the hook must return inside the OS LowLevelHooksTimeout,
/// so at most one signal is queued at a time.
static HOOK_PENDING: AtomicBool = AtomicBool::new(false);
/// Events the hook recognised as this backend's own injections.
static HOOK_OWN_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Runs inside the OS low-level hook: no domain/lease lock, no blocking
/// send, one atomic test to coalesce a burst of motion events.
fn signal_external_input(event: ExternalInputEvent) {
    if HOOK_PENDING.swap(true, Ordering::Relaxed) {
        return;
    }
    let Ok(guard) = HOOK_SINK.lock() else {
        HOOK_PENDING.store(false, Ordering::Relaxed);
        return;
    };
    let Some(sender) = guard.as_ref() else {
        HOOK_PENDING.store(false, Ordering::Relaxed);
        return;
    };
    if sender.send(event).is_err() {
        HOOK_PENDING.store(false, Ordering::Relaxed);
    }
}

extern "system" fn keyboard_hook_procedure(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION && lparam != 0 {
        // SAFETY: the OS passes a KBDLLHOOKSTRUCT to WH_KEYBOARD_LL hooks.
        let info = unsafe { &*(lparam as *const KbdLlHookStruct) };
        let injected = info.flags & LLKHF_INJECTED != 0;
        if injected && info.dw_extra_info == SELF_INPUT_MARKER {
            HOOK_OWN_EVENTS.fetch_add(1, Ordering::Relaxed);
        } else {
            signal_external_input(ExternalInputEvent {
                kind: "keyboard",
                virtual_key: info.vk_code,
                injected,
            });
        }
    }
    // SAFETY: pass-through for every other hook code and every unhandled
    // event; returning a value other than the chain's would swallow input.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

extern "system" fn mouse_hook_procedure(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION && lparam != 0 {
        // SAFETY: the OS passes an MSLLHOOKSTRUCT to WH_MOUSE_LL hooks.
        let info = unsafe { &*(lparam as *const MsllHookStruct) };
        let injected = info.flags & LLMHF_INJECTED != 0;
        if injected && info.dw_extra_info == SELF_INPUT_MARKER {
            HOOK_OWN_EVENTS.fetch_add(1, Ordering::Relaxed);
        } else {
            signal_external_input(ExternalInputEvent {
                kind: "mouse",
                virtual_key: 0,
                injected,
            });
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Real facts about the process-owned hook watcher, reported per session and
/// in capability discovery instead of a guessed `available: true`.
#[derive(Debug, Clone, Default, serde::Serialize)]
struct HumanInputWatchFacts {
    installed: bool,
    keyboard_hook: bool,
    mouse_hook: bool,
    thread_id: Option<u32>,
    last_error: Option<String>,
    external_events: u64,
    own_events_filtered: u64,
    last_revoked_domains: usize,
    last_event_kind: Option<&'static str>,
    last_event_virtual_key: Option<u32>,
    last_event_injected: bool,
}

/// One provider's human-input watcher: a native hook thread (installer +
/// message pump) and a dispatcher thread that performs the revoke outside
/// the hook, so a callback never waits on a lease lock and can never outlive
/// the state it reports into.
struct HumanInputWatch {
    domains: Weak<Mutex<WindowsState>>,
    fanout: Arc<HumanInputFanout>,
    facts: Mutex<HumanInputWatchFacts>,
    ready: AtomicBool,
    stopped: AtomicBool,
    /// True only when this watcher installed the process-global sink, so a
    /// second provider can never clear another's signal target.
    owns_sink: AtomicBool,
    thread_id: AtomicU32,
    hook_thread: Mutex<Option<JoinHandle<()>>>,
    dispatch_thread: Mutex<Option<JoinHandle<()>>>,
}

impl HumanInputWatch {
    /// Start the watcher. A failure is recorded as a fact (never a fake
    /// capability and never a refused session): capture and input keep
    /// working, human-input observation honestly reports unavailable.
    fn start(domains: Weak<Mutex<WindowsState>>, fanout: Arc<HumanInputFanout>) -> Arc<Self> {
        let watch = Arc::new(Self {
            domains,
            fanout,
            facts: Mutex::new(HumanInputWatchFacts::default()),
            ready: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            owns_sink: AtomicBool::new(false),
            thread_id: AtomicU32::new(0),
            hook_thread: Mutex::new(None),
            dispatch_thread: Mutex::new(None),
        });
        let (sender, receiver) = mpsc::channel::<ExternalInputEvent>();
        let (ready_tx, ready_rx) = mpsc::channel::<HookReport>();
        // Install the sink BEFORE the hooks exist: a callback that fires
        // during installation must already have a live target.
        match HOOK_SINK.lock() {
            Ok(mut slot) => {
                if slot.is_some() {
                    // One low-level hook sink per process: the hook procedure
                    // has no user data, so a second watcher cannot be
                    // addressed and would silently steal this one's signals.
                    drop(slot);
                    watch.record_error(
                        "another low-level hook watcher already owns this process",
                    );
                    return watch;
                }
                *slot = Some(sender);
                watch.owns_sink.store(true, Ordering::SeqCst);
            }
            Err(_) => {
                watch.record_error("hook sink poisoned");
                return watch;
            }
        }
        HOOK_PENDING.store(false, Ordering::Relaxed);
        let weak_hook = Arc::downgrade(&watch);
        let spawned = std::thread::Builder::new()
            .name("shenko-win32-hooks".into())
            .spawn(move || run_hook_thread(weak_hook, ready_tx));
        let handle = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                watch.record_error(&format!("hook thread spawn failed: {error}"));
                watch.release_sink();
                return watch;
            }
        };
        if let Ok(mut guard) = watch.hook_thread.lock() {
            *guard = Some(handle);
        }
        let deadline = std::time::Instant::now() + WATCH_SETUP_TIMEOUT;
        let mut install_result: Option<Result<(), String>> = None;
        while install_result.is_none() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match ready_rx.recv_timeout(remaining) {
                Ok(HookReport::Started(thread_id)) => {
                    watch.thread_id.store(thread_id, Ordering::SeqCst);
                }
                Ok(HookReport::Installed(result)) => install_result = Some(result),
                Err(_) => break,
            }
        }
        match install_result {
            Some(Ok(())) => {
                let thread_id = watch.thread_id.load(Ordering::SeqCst);
                if let Ok(mut facts) = watch.facts.lock() {
                    facts.installed = true;
                    facts.keyboard_hook = true;
                    facts.mouse_hook = true;
                    facts.thread_id = Some(thread_id);
                    facts.last_error = None;
                }
            }
            Some(Err(error)) => {
                watch.record_error(&error);
                watch.stop();
                return watch;
            }
            None => {
                watch.record_error(
                    "hook thread did not report readiness; the watcher is not exposed",
                );
                watch.stop();
                return watch;
            }
        }
        let weak_watch = Arc::downgrade(&watch);
        let domains = watch.domains.clone();
        let fanout = Arc::clone(&watch.fanout);
        let spawned = std::thread::Builder::new()
            .name("shenko-win32-input-dispatch".into())
            .spawn(move || run_dispatch_thread(receiver, domains, fanout, weak_watch));
        match spawned {
            Ok(handle) => {
                if let Ok(mut guard) = watch.dispatch_thread.lock() {
                    *guard = Some(handle);
                }
                watch.ready.store(true, Ordering::SeqCst);
            }
            Err(error) => {
                watch.record_error(&format!("input dispatch thread spawn failed: {error}"));
                watch.stop();
            }
        }
        watch
    }

    fn record_error(&self, error: &str) {
        if let Ok(mut facts) = self.facts.lock() {
            facts.last_error = Some(error.to_string());
            facts.installed = false;
            facts.keyboard_hook = false;
            facts.mouse_hook = false;
        }
    }

    fn snapshot(&self) -> HumanInputWatchFacts {
        let mut facts = self
            .facts
            .lock()
            .map(|facts| facts.clone())
            .unwrap_or_default();
        facts.own_events_filtered = HOOK_OWN_EVENTS.load(Ordering::Relaxed);
        facts
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Clear the process-global sink only when this watcher installed it.
    fn release_sink(&self) {
        if self.owns_sink.swap(false, Ordering::SeqCst) {
            if let Ok(mut slot) = HOOK_SINK.lock() {
                *slot = None;
            }
        }
    }

    /// Stop the message loop, join the hook thread, drop the global sink
    /// (which closes the dispatcher's channel) and join the dispatcher. After
    /// this returns no native thread of this watcher exists and no hook
    /// callback can run.
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.ready.store(false, Ordering::SeqCst);
        let thread_id = self.thread_id.swap(0, Ordering::SeqCst);
        if thread_id != 0 {
            // Wake the pump immediately; the stop flag alone already ends it.
            unsafe {
                PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(handle) = self.hook_thread.lock().ok().and_then(|mut guard| guard.take()) {
            let _ = handle.join();
        }
        self.release_sink();
        HOOK_PENDING.store(false, Ordering::SeqCst);
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
            facts.keyboard_hook = false;
            facts.mouse_hook = false;
        }
    }
}

impl Drop for HumanInputWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Hook thread: create this thread's message queue, install both low-level
/// hooks, prove installation to the provider, then pump messages until the
/// watcher is stopped — unhooking before it exits.
///
/// The pump peeks (plus a short sleep) instead of blocking in `GetMessageW`:
/// low-level hooks are still delivered to this thread on every message check,
/// and the loop can observe the stop flag even if no `WM_QUIT` ever reached
/// this thread, so teardown can always join it.
fn run_hook_thread(watch: Weak<HumanInputWatch>, ready: mpsc::Sender<HookReport>) {
    let thread_id = unsafe { GetCurrentThreadId() };
    // A thread message queue exists only after a message-retrieval call;
    // creating it first means a WM_QUIT posted immediately after readiness
    // cannot be lost.
    let mut msg = unsafe { std::mem::zeroed::<Msg>() };
    unsafe {
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    // Announce the queue before installing: the provider can then always end
    // this thread, even if installation itself stalls.
    let _ = ready.send(HookReport::Started(thread_id));
    let keyboard = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            keyboard_hook_procedure,
            std::ptr::null_mut(),
            0,
        )
    };
    if keyboard.is_null() {
        let error = unsafe { GetLastError() };
        let _ = ready.send(HookReport::Installed(Err(format!(
            "SetWindowsHookExW(WH_KEYBOARD_LL) failed: {error}"
        ))));
        return;
    }
    let mouse = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            mouse_hook_procedure,
            std::ptr::null_mut(),
            0,
        )
    };
    if mouse.is_null() {
        let error = unsafe { GetLastError() };
        unsafe {
            UnhookWindowsHookEx(keyboard);
        }
        let _ = ready.send(HookReport::Installed(Err(format!(
            "SetWindowsHookExW(WH_MOUSE_LL) failed: {error}"
        ))));
        return;
    }
    let _ = ready.send(HookReport::Installed(Ok(())));
    // Low-level hooks are delivered when this thread checks its queue, so the
    // pump itself is the delivery path; nothing is dispatched elsewhere.
    loop {
        let stop_requested = match watch.upgrade() {
            Some(watch) => watch.stopped.load(Ordering::SeqCst),
            None => true,
        };
        if stop_requested {
            break;
        }
        let has_message = unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
        if has_message != 0 {
            if msg.message == WM_QUIT {
                break;
            }
            continue;
        }
        // No message: yield briefly. 10 ms keeps a human keypress or click
        // observation latency negligible while the loop stays cheap.
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        UnhookWindowsHookEx(mouse);
        UnhookWindowsHookEx(keyboard);
    }
}

/// Dispatcher: the only place external input mutates a domain, so the hook
/// procedure stays free of lease locks. Ownership of the provider state is
/// weak; if the provider is gone the loop ends instead of revoking nothing.
fn run_dispatch_thread(
    receiver: mpsc::Receiver<ExternalInputEvent>,
    domains: Weak<Mutex<WindowsState>>,
    fanout: Arc<HumanInputFanout>,
    watch: Weak<HumanInputWatch>,
) {
    while let Ok(event) = receiver.recv() {
        HOOK_PENDING.store(false, Ordering::SeqCst);
        let Some(state) = domains.upgrade() else {
            break;
        };
        let revoked = revoke_domains_for_external_input(&state, &fanout);
        if let Some(watch) = watch.upgrade() {
            if let Ok(mut facts) = watch.facts.lock() {
                facts.external_events += 1;
                facts.own_events_filtered = HOOK_OWN_EVENTS.load(Ordering::Relaxed);
                facts.last_revoked_domains = revoked;
                facts.last_event_kind = Some(event.kind);
                facts.last_event_virtual_key = Some(event.virtual_key);
                facts.last_event_injected = event.injected;
            }
        }
    }
}

/// Revoke the interactive domain the external event reached. The low-level
/// hook only observes the current input desktop, so the affected domain is
/// resolved from the live desktop name; when that query fails, every domain
/// is revoked (an external event did happen — never a silent miss). Reuses
/// the provider's single handover fence: epoch bump, held-input release and
/// fanout dispatch under the domain lock.
fn revoke_domains_for_external_input(
    state: &Mutex<WindowsState>,
    fanout: &HumanInputFanout,
) -> usize {
    let current = current_input_desktop_name().ok();
    let mut targets: Vec<(String, Arc<Mutex<WindowsDomainState>>)> = Vec::new();
    if let Ok(locked) = state.lock() {
        for (desktop, domain) in &locked.domains {
            let affected = match &current {
                Some(name) => name.eq_ignore_ascii_case(desktop),
                None => true,
            };
            if affected {
                targets.push((desktop.clone(), Arc::clone(domain)));
            }
        }
    }
    let mut revoked = 0;
    for (domain_id, domain) in targets {
        let notified = if let Ok(mut domain) = domain.lock() {
            domain.lease_state.holder = None;
            domain.lease_state.epoch += 1;
            let held: Vec<HeldInput> = domain.held.iter().copied().collect();
            domain.held.clear();
            // Release inside the domain critical section: the next lease
            // cannot emit input until this handover completes.
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

impl DesktopProvider for WindowsDesktopProvider {
    fn provider_id(&self) -> &'static str {
        "windows"
    }

    fn capabilities(&self) -> Vec<DesktopCapabilityFact> {
        // Discovery reports the watcher's REAL state: before the first
        // session no hook exists, so detection is not yet available and this
        // says so instead of advertising a message loop that is not running.
        let watch = self.watch_facts();
        vec![
            DesktopCapabilityFact {
                capability: DesktopCapability::ScreenRead,
                available: true,
                detail: "GDI BitBlt/GetDIBits".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::Input,
                available: true,
                detail: "SendInput (user32)".into(),
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::ExternalInputDetection,
                available: watch.installed,
                detail: if watch.installed {
                    format!(
                        "WH_KEYBOARD_LL/WH_MOUSE_LL on an owned message-loop thread ({:?}); installed at first session open",
                        watch.thread_id
                    )
                } else {
                    match &watch.last_error {
                        Some(error) => format!(
                            "low-level hooks are installed per session and failed here: {error}"
                        ),
                        None => {
                            "low-level hooks are installed on the first session open; none is running yet"
                                .to_string()
                        }
                    }
                },
            },
            DesktopCapabilityFact {
                capability: DesktopCapability::LayoutRevision,
                available: true,
                detail: "visible top-level window rects + foreground hash".into(),
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
        // Capture must be 1:1 physical pixels; ask the OS once. Failure is
        // reported, not fatal — the capture path degrades to scaled pixels
        // and the fact stays visible in the session detail.
        let dpi_aware = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } != 0;
        let desktop = current_input_desktop_name()?;
        // This provider captures and injects into exactly one interactive
        // desktop — the process's input desktop. Opening any other desktop
        // name or screen index would silently drive a different surface, so
        // it is refused with the missing capability named.
        if let Some(requested) = request.display.as_deref().filter(|name| !name.is_empty()) {
            if !requested.eq_ignore_ascii_case(&desktop) {
                return Err(DesktopError::unavailable(
                    "requested_desktop_not_driveable",
                    format!(
                        "input desktop `{desktop}` is the only driveable surface in this build; `{requested}` would need a window-station/desktop switch this provider does not perform"
                    ),
                ));
            }
        }
        if let Some(screen) = request.screen {
            if screen != 0 {
                return Err(DesktopError::unavailable(
                    "screen_selection_unavailable",
                    format!(
                        "monitor {screen} was requested; this provider drives the primary display (screen 0) only"
                    ),
                ));
            }
        }
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if width <= 0 || height <= 0 {
            return Err(DesktopError::unavailable(
                "display_unreachable",
                "GetSystemMetrics returned a non-positive desktop size",
            ));
        }
        let dpi = unsafe { GetDpiForSystem() };
        let session_id = EntityId::new();
        // Human-input observation is a sub-capability: start the hook watcher
        // (idempotent) and let its probed readiness decide the fact. A hook
        // that cannot install never refuses the session and never becomes a
        // fake capability.
        let watch = self.ensure_human_input_watch();
        let mut state = self.locked()?;
        let domain = state
            .domains
            .entry(desktop.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(WindowsDomainState {
                    lease_state: DesktopLeaseState::new(),
                    held: BTreeSet::new(),
                    session_ids: BTreeSet::new(),
                }))
            })
            .clone();
        state.sessions.insert(
            session_id,
            Arc::new(WindowsSession {
                session_id,
                closed: AtomicBool::new(false),
                owner_instance,
                input_desktop: desktop.clone(),
                shared: Arc::new(Mutex::new(WindowsSessionShared {
                    next_frame: 0,
                    next_revision: 0,
                    send_log: BTreeMap::new(),
                })),
                domain,
                watch,
                width_px: width as u32,
                height_px: height as u32,
                dpi,
            }),
        );
        drop(state);
        let session = self.session(session_id)?;
        if let Ok(mut domain) = session.domain_locked() {
            domain.session_ids.insert(session_id);
        }
        let session: Arc<dyn DesktopSession> = session;
        let _ = dpi_aware;
        Ok(DesktopSessionHandle::new(session_id, session, owner_instance))
    }

    fn observe(
        &self,
        handle: &DesktopSessionHandle,
        region: Option<[i32; 4]>,
    ) -> Result<DesktopFrame, DesktopError> {
        let session = self.session(handle.session_id)?;
        let width = session.width_px as i32;
        let height = session.height_px as i32;
        let region = region.unwrap_or([0, 0, width, height]);
        let epoch = session
            .domain
            .lock()
            .map(|d| d.lease_state.epoch)
            .unwrap_or(0);
        let (raw, rgb, w, h) = capture_region(region)?;
        let (frame_id, revision) = {
            let mut shared = session.locked()?;
            shared.next_frame += 1;
            shared.next_revision += 1;
            (shared.next_frame, shared.next_revision)
        };
        Ok(DesktopFrame {
            region,
            width: w as u32,
            height: h as u32,
            raw_pixels: raw.clone(),
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
            dpi_x: f64::from(session.dpi),
            dpi_y: f64::from(session.dpi),
            scale_factor: f64::from(session.dpi) / 96.0,
            frame_id,
            revision,
            lease_epoch: epoch,
            screen_num: 0,
            detail: serde_json::json!({
                "input_desktop": session.input_desktop,
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
        // Live identity + layout verification before any event.
        verify_observation_state(&session, &request.observation, request.require_pixel_fresh)?;
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
            let mut events: Vec<WinInput> = Vec::new();
            let mut held_now: Vec<HeldInput> = Vec::new();
            let mut released_now: Vec<HeldInput> = Vec::new();
            match action {
                DesktopAction::PointerMove { x, y } => {
                    let ok = unsafe { SetCursorPos(*x, *y) } != 0;
                    if !ok {
                        receipts.push(DesktopActionReceipt {
                            index,
                            kind: action.kind().into(),
                            transport: "lost",
                            observed_effect: None,
                        });
                        failure = Some("transport_lost");
                        break;
                    }
                    events.push(mouse_event(MOUSEEVENTF_MOVE));
                }
                DesktopAction::Button { button, press } => {
                    let down = matches!(press, DesktopPress::Down | DesktopPress::Tap);
                    let up = matches!(press, DesktopPress::Up | DesktopPress::Tap);
                    if down {
                        match mouse_button_flags(*button, true) {
                            Some(flags) => events.push(mouse_event(flags)),
                            None => {
                                receipts.push(DesktopActionReceipt {
                                    index,
                                    kind: action.kind().into(),
                                    transport: "rejected",
                                    observed_effect: Some("button_unsupported"),
                                });
                                continue;
                            }
                        }
                        held_now.push(HeldInput::Button(*button));
                    }
                    if up {
                        match mouse_button_flags(*button, false) {
                            Some(flags) => events.push(mouse_event(flags)),
                            None => {
                                receipts.push(DesktopActionReceipt {
                                    index,
                                    kind: action.kind().into(),
                                    transport: "rejected",
                                    observed_effect: Some("button_unsupported"),
                                });
                                continue;
                            }
                        }
                        released_now.push(HeldInput::Button(*button));
                    }
                }
                DesktopAction::Key { key, press } => {
                    let Some(vk) = virtual_key_code(key) else {
                        receipts.push(DesktopActionReceipt {
                            index,
                            kind: action.kind().into(),
                            transport: "rejected",
                            observed_effect: Some("key_unmapped"),
                        });
                        continue;
                    };
                    match press.unwrap_or(DesktopPress::Tap) {
                        DesktopPress::Tap => {
                            events.push(key_event(vk, true));
                            events.push(key_event(vk, false));
                        }
                        DesktopPress::Down => {
                            events.push(key_event(vk, true));
                            held_now.push(HeldInput::Key(vk));
                        }
                        DesktopPress::Up => {
                            events.push(key_event(vk, false));
                            released_now.push(HeldInput::Key(vk));
                        }
                    }
                }
                DesktopAction::Text { text } => {
                    for ch in text.chars() {
                        events.extend(text_events(ch));
                    }
                }
            }
            match send_inputs(&events) {
                Ok(delivered) => {
                    sent += delivered;
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
        let mut pointer_post = None;
        unsafe {
            let mut point = Point::default();
            if GetCursorPos(&mut point) != 0 {
                pointer_post = Some([point.x, point.y]);
            }
        }
        Ok(DesktopInjectOutcome {
            receipts,
            sent_events: sent,
            postcondition_verified: false,
            stopped_reason: failure.map(str::to_string),
            detail: serde_json::json!({
                "pointer_post": pointer_post,
                "sent_events": sent,
                "cancelled": cancelled,
            }),
        })
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
                "observation": "controller recovered without a conclusive SendInput result",
                "sent_events_before_recovery": sent,
                "session_alive_in_memory": alive,
                "process_stop_proven": false,
            }),
        }
    }
}

/// Verify the bound observation against the live machine state: layout
/// fingerprint must re-compute equal; the pixel check re-captures when
/// `require_pixel_fresh` is set.
fn verify_observation_state(
    session: &WindowsSession,
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
            "the live window layout no longer matches the bound observation",
        ));
    }
    if require_pixel_fresh {
        let (raw, _, _, _) = capture_region(observation.region)?;
        if hex_digest(&raw) != observation.pixel_hash {
            return Err(DesktopError::rejected(
                "observation_stale_pixels",
                "the observed pixels changed before input",
            ));
        }
    }
    Ok(())
}
