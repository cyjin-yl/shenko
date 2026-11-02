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

#![cfg(target_os = "windows")]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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
                    // A real LLMHF hook tap needs this process to own a
                    // message loop; it is not guessed here.
                    available: false,
                    detail: "requires a low-level hook message loop owned by this process; takeover stays lease-bound".into(),
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
    state: Mutex<WindowsState>,
}

impl WindowsDesktopProvider {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(WindowsState {
                sessions: BTreeMap::new(),
                domains: BTreeMap::new(),
            }),
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
                dw_extra_info: 0,
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
                        dw_extra_info: 0,
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
                dw_extra_info: 0,
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

impl DesktopProvider for WindowsDesktopProvider {
    fn provider_id(&self) -> &'static str {
        "windows"
    }

    fn capabilities(&self) -> Vec<DesktopCapabilityFact> {
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
                available: false,
                detail: "needs a low-level hook message loop owned by this process".into(),
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
        _request: &DesktopOpenRequest,
        owner_instance: EntityId,
    ) -> Result<DesktopSessionHandle, DesktopError> {
        // Capture must be 1:1 physical pixels; ask the OS once. Failure is
        // reported, not fatal — the capture path degrades to scaled pixels
        // and the fact stays visible in the session detail.
        let dpi_aware = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } != 0;
        let desktop = current_input_desktop_name()?;
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
