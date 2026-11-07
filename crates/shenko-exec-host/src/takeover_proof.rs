//! Shared helpers for the Computer Use external-device takeover proof.
//!
//! These are the reusable building blocks that both the low-level native
//! harness (`computer_use_takeover_native`) and the public default-CLI
//! consumer test (`computer_use_takeover_cli`) depend on.
//!
//! # Modules
//!
//! * [`uinput`] — Create a genuine kernel keyboard via `/dev/uinput`,
//!   await its `/sys/class/input/eventN` node, and emit real `input_event`
//!   records. The kernel device name is unique per run.
//! * [`owned_xorg`] — Spawn an isolated Xorg server with the `dummy`
//!   video driver and the `evdev` input driver bound to exactly one
//!   synthetic keyboard, no ambient devices. Cleanup is PID-checked.
//! * [`Monitor`] — An independent XI2 raw-event client on its own X11
//!   connection; it observes what the server delivers but never injects.
//! * [`EvdevReader`] — Read raw `input_event` records from a kernel evdev
//!   node, independent of Xorg.
//! * [`Probe`] — Independent `XQueryPointer`/`XQueryKeymap` on its own
//!   connection, so no provider call is trusted for the X server's state.
//! * [`DispatchRecorder`] — Records the provider's real `HumanInputFanout`
//!   dispatch without triggering or faking a revocation.
//!
//! # Honesty boundaries
//!
//! The device is a *synthetic kernel device*. This proves the non-XTEST
//! external-input revocation mechanism. It is NOT a claim that a physical
//! human pressed a key: evidence carries `human_intent_claim: false`.
//!
//! # Prerequisite gating
//!
//! Missing capabilities (uinput, Xorg, drivers) fail RED as inconclusive —
//! they never skip-green.
#![cfg(target_os = "linux")]

use crate::{HumanInputObserver};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xinput::{self, XIEventMask},
        xproto,
    },
    rust_connection::RustConnection,
};

/// XIAllMasterDevices: raw events for every device feed on the domain.
pub const XI_ALL_MASTER_DEVICES: u16 = 1;
/// XIAllDevices: enumerate the whole hierarchy.
pub const XI_ALL_DEVICES: u16 = 0;
/// Core pointer `Button1Mask` (X11 protocol, not an XI2 fact).
pub const BUTTON1_MASK: u16 = 0x0100;
/// Default Xorg readiness timeout.
pub const XORG_READY_TIMEOUT: Duration = Duration::from_secs(25);
/// Default event delivery timeout.
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(8);
/// Default X state convergence timeout.
pub const STATE_TIMEOUT: Duration = Duration::from_secs(5);
/// Default kernel node appearance timeout.
pub const NODE_TIMEOUT: Duration = Duration::from_secs(5);

/// `EV_KEY` event type in the kernel input subsystem.
pub const EV_KEY: u16 = 1;
/// `KEY_F13` evdev code: a function key with no default desktop binding.
pub const KEY_F13_EVDEV: u16 = 183;
/// Size of a 64-bit `struct input_event` in bytes.
pub const INPUT_EVENT_SIZE: usize = 24;
/// `O_NONBLOCK` for evdev reader.
pub const O_NONBLOCK: i32 = 0o4000;

// ---------------------------------------------------------------------------
// Raw event / kernel event fact records (serializable evidence)
// ---------------------------------------------------------------------------

/// A single XI2 raw event as observed by the independent [`Monitor`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RawEventFacts {
    pub kind: String,
    pub sourceid: u16,
    pub detail: Option<u32>,
}

/// A single kernel `input_event` as read by the independent [`EvdevReader`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KernelEventFacts {
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
}

// ---------------------------------------------------------------------------
// Independent XI2 raw-event client
// ---------------------------------------------------------------------------

/// An independent XI2 raw-event monitor on its own X11 connection.
///
/// It observes what the server delivers but never injects or calls the
/// provider. The input driver attaches asynchronously to the socket being
/// ready, so callers should retry [`Monitor::open`] within a bounded window
/// when the device is not yet admitted.
pub struct Monitor {
    conn: RustConnection,
    pub major: u16,
    pub minor: u16,
    pub device_id: u16,
    pub device_name: String,
    pub xtest_ids: Vec<u16>,
}

impl Monitor {
    /// Open an independent XI2 raw-event connection to `display` and locate
    /// the kernel device named `expected_device` in the XInput2 hierarchy.
    pub fn open(display: &str, expected_device: &str) -> Result<Self, owned_xorg::Fault> {
        let (conn, screen_num) = RustConnection::connect(Some(display)).map_err(|error| {
            owned_xorg::Fault::new("monitor_connect", error.to_string())
        })?;
        let root = conn.setup().roots[screen_num].root;
        let version = xinput::xi_query_version(&conn, 2, 3)
            .map_err(|error| owned_xorg::Fault::new("xi_query_version_transport", error.to_string()))?
            .reply()
            .map_err(|error| owned_xorg::Fault::new("xi_query_version_reply", error.to_string()))?;
        let devices = xinput::xi_query_device(&conn, XI_ALL_DEVICES)
            .map_err(|error| owned_xorg::Fault::new("xi_query_device_transport", error.to_string()))?
            .reply()
            .map_err(|error| owned_xorg::Fault::new("xi_query_device_reply", error.to_string()))?;
        let mut device_id = None;
        let mut device_name = String::new();
        let mut device_enabled = false;
        let mut xtest_ids = vec![];
        for device in &devices.infos {
            let name = String::from_utf8_lossy(&device.name).into_owned();
            if name.to_ascii_uppercase().contains("XTEST") {
                xtest_ids.push(device.deviceid);
            }
            if name == expected_device {
                device_id = Some(device.deviceid);
                device_name = name;
                device_enabled = device.enabled;
            }
        }
        let Some(device_id) = device_id else {
            return Err(owned_xorg::Fault::new(
                "xorg_input_device_not_admitted",
                format!(
                    "XInput2 hierarchy does not contain the kernel device {expected_device:?} (devices: {})",
                    devices
                        .infos
                        .iter()
                        .map(|device| format!(
                            "{}={}",
                            device.deviceid,
                            String::from_utf8_lossy(&device.name)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        };
        if !device_enabled {
            return Err(owned_xorg::Fault::new(
                "xorg_input_device_disabled",
                format!(
                    "kernel device {expected_device:?} is present but disabled in the XInput2 hierarchy"
                ),
            ));
        }
        let mut mask_bits = u32::from(XIEventMask::RAW_KEY_PRESS)
            | u32::from(XIEventMask::RAW_KEY_RELEASE)
            | u32::from(XIEventMask::RAW_BUTTON_PRESS)
            | u32::from(XIEventMask::RAW_BUTTON_RELEASE)
            | u32::from(XIEventMask::RAW_MOTION);
        if version.major_version > 2 || (version.major_version == 2 && version.minor_version >= 2) {
            mask_bits |= u32::from(XIEventMask::RAW_TOUCH_BEGIN)
                | u32::from(XIEventMask::RAW_TOUCH_UPDATE)
                | u32::from(XIEventMask::RAW_TOUCH_END);
        }
        let masks = [xinput::EventMask {
            deviceid: XI_ALL_MASTER_DEVICES,
            mask: vec![XIEventMask::from(mask_bits)],
        }];
        xinput::xi_select_events(&conn, root, &masks)
            .map_err(|error| owned_xorg::Fault::new("xi_select_events_transport", error.to_string()))?
            .check()
            .map_err(|error| owned_xorg::Fault::new("xi_select_events_rejected", error.to_string()))?;
        Ok(Self {
            conn,
            major: version.major_version,
            minor: version.minor_version,
            device_id,
            device_name,
            xtest_ids,
        })
    }

    /// Collect every raw event already queued on this independent
    /// connection. Never injects; never calls the provider.
    pub fn drain_into(&self, out: &mut Vec<RawEventFacts>) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::XinputRawKeyPress(event))) => out.push(RawEventFacts {
                    kind: "raw_key_press".into(),
                    sourceid: event.sourceid,
                    detail: Some(event.detail),
                }),
                Ok(Some(Event::XinputRawKeyRelease(event))) => out.push(RawEventFacts {
                    kind: "raw_key_release".into(),
                    sourceid: event.sourceid,
                    detail: Some(event.detail),
                }),
                Ok(Some(Event::XinputRawButtonPress(event))) => out.push(RawEventFacts {
                    kind: "raw_button_press".into(),
                    sourceid: event.sourceid,
                    detail: Some(event.detail),
                }),
                Ok(Some(Event::XinputRawButtonRelease(event))) => out.push(RawEventFacts {
                    kind: "raw_button_release".into(),
                    sourceid: event.sourceid,
                    detail: Some(event.detail),
                }),
                Ok(Some(other)) => out.push(RawEventFacts {
                    kind: format!("other:{:?}", std::mem::discriminant(&other)),
                    sourceid: 0,
                    detail: None,
                }),
                Ok(None) => return,
                Err(error) => {
                    out.push(RawEventFacts {
                        kind: format!("transport_error:{error}"),
                        sourceid: 0,
                        detail: None,
                    });
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fanout observer: records the provider's real dispatch, never triggers it
// ---------------------------------------------------------------------------

/// Records the provider's real `HumanInputFanout` dispatch without
/// triggering or faking a revocation.
pub struct DispatchRecorder {
    domains: Mutex<Vec<String>>,
    changed: Condvar,
}

impl Default for DispatchRecorder {
    fn default() -> Self {
        Self { domains: Mutex::new(Vec::new()), changed: Condvar::new() }
    }
}

impl DispatchRecorder {
    /// Wait until at least one domain is dispatched, or `timeout` elapses.
    pub fn wait(&self, timeout: Duration) -> Vec<String> {
        let started = Instant::now();
        let mut domains = self.domains.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if !domains.is_empty() || started.elapsed() >= timeout {
                return domains.clone();
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            let (guard, _) = self
                .changed
                .wait_timeout(domains, remaining)
                .unwrap_or_else(|error| error.into_inner());
            domains = guard;
        }
    }
}

impl HumanInputObserver for DispatchRecorder {
    fn on_human_input(&self, domain_id: &str) {
        let mut domains = self.domains.lock().unwrap_or_else(|error| error.into_inner());
        if !domains.iter().any(|domain| domain == domain_id) {
            domains.push(domain_id.to_string());
        }
        drop(domains);
        self.changed.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Kernel evdev reader (independent of X)
// ---------------------------------------------------------------------------

/// Reads raw `input_event` records from a kernel evdev node.
///
/// Independent of Xorg: the device is NOT grabbed (`GrabDevice false`),
/// so both the server and this reader observe the very same kernel events.
pub struct EvdevReader {
    file: std::fs::File,
    carry: Vec<u8>,
}

impl EvdevReader {
    /// Open the evdev node at `node` in non-blocking mode.
    pub fn open(node: &Path) -> Result<Self, owned_xorg::Fault> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(node)
            .map_err(|error| {
                owned_xorg::Fault::new("evdev_read_open", format!("{}: {error}", node.display()))
            })?;
        Ok(Self { file, carry: vec![] })
    }

    /// Drain all available events without blocking.
    pub fn drain(&mut self) -> Vec<KernelEventFacts> {
        use std::io::Read;
        let mut events = vec![];
        let mut buffer = [0u8; INPUT_EVENT_SIZE * 16];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    self.carry.extend_from_slice(&buffer[..read]);
                    while self.carry.len() >= INPUT_EVENT_SIZE {
                        let record: Vec<u8> = self.carry.drain(..INPUT_EVENT_SIZE).collect();
                        events.push(KernelEventFacts {
                            event_type: u16::from_ne_bytes([record[16], record[17]]),
                            code: u16::from_ne_bytes([record[18], record[19]]),
                            value: i32::from_ne_bytes([
                                record[20], record[21], record[22], record[23],
                            ]),
                        });
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    events.push(KernelEventFacts {
                        event_type: u16::MAX,
                        code: 0,
                        value: error.raw_os_error().unwrap_or(0),
                    });
                    break;
                }
            }
        }
        events
    }
}

// ---------------------------------------------------------------------------
// Independent X state checks (own connections, standard protocol)
// ---------------------------------------------------------------------------

/// State of held keys/buttons after injection, before revocation.
#[derive(Debug, Clone)]
pub struct HeldState {
    pub button1_down: bool,
    pub held_keycodes: Vec<u16>,
}

/// State of held keys/buttons after revocation and release.
#[derive(Debug, Clone)]
pub struct ReleasedState {
    pub button1_down: bool,
    pub held_keycodes: Vec<u16>,
    pub elapsed_ms: u64,
}

/// Independent `XQueryPointer`/`XQueryKeymap` on its own connection.
pub struct Probe {
    conn: RustConnection,
    root: u32,
}

impl Probe {
    /// Open a fresh X11 connection for state probing.
    pub fn connect(display: &str) -> Result<Self, owned_xorg::Fault> {
        let (conn, screen_num) = RustConnection::connect(Some(display)).map_err(|error| {
            owned_xorg::Fault::new("x11_probe_connect", error.to_string())
        })?;
        let root = conn.setup().roots[screen_num].root;
        Ok(Self { conn, root })
    }

    /// Query the pointer position on the root window.
    pub fn pointer(&self) -> Result<[i16; 2], owned_xorg::Fault> {
        let reply = xproto::query_pointer(&self.conn, self.root)
            .map_err(|error| owned_xorg::Fault::new("xquerypointer_transport", error.to_string()))?
            .reply()
            .map_err(|error| owned_xorg::Fault::new("xquerypointer_reply", error.to_string()))?;
        Ok([reply.root_x, reply.root_y])
    }

    /// Whether button 1 is currently down.
    pub fn button1_down(&self) -> Result<bool, owned_xorg::Fault> {
        let reply = xproto::query_pointer(&self.conn, self.root)
            .map_err(|error| owned_xorg::Fault::new("xquerypointer_transport", error.to_string()))?
            .reply()
            .map_err(|error| owned_xorg::Fault::new("xquerypointer_reply", error.to_string()))?;
        Ok(u16::from(reply.mask) & BUTTON1_MASK != 0)
    }

    /// Read the full 256-key keymap from the X server.
    pub fn keymap(&self) -> Result<[u8; 32], owned_xorg::Fault> {
        let reply = xproto::query_keymap(&self.conn)
            .map_err(|error| owned_xorg::Fault::new("xquerykeymap_transport", error.to_string()))?
            .reply()
            .map_err(|error| owned_xorg::Fault::new("xquerykeymap_reply", error.to_string()))?;
        Ok(reply.keys)
    }
}

/// Extract keycodes that are currently down from a keymap.
pub fn down_keycodes(keys: &[u8; 32]) -> Vec<u16> {
    let mut codes = vec![];
    for (index, byte) in keys.iter().enumerate() {
        for bit in 0..8u16 {
            if byte & (1u8 << bit) != 0 {
                let code = index as u16 * 8 + bit;
                if code != 0 {
                    codes.push(code);
                }
            }
        }
    }
    codes
}

/// Keycodes that the injection turned on: the provider may legitimately
/// remap an unmapped keysym, so the harness reads the actual diff instead of
/// assuming a keycode.
pub fn newly_down(before: &[u8; 32], after: &[u8; 32]) -> Vec<u16> {
    let mut keys = [0u8; 32];
    for index in 0..32 {
        keys[index] = after[index] & !before[index];
    }
    down_keycodes(&keys)
}

/// Wait until the X server shows the injected button/key as held, or
/// `timeout` elapses.
pub fn wait_held_state(probe: &Probe, baseline: &[u8; 32], timeout: Duration) -> Option<HeldState> {
    let started = Instant::now();
    loop {
        let button1_down = probe.button1_down().ok()?;
        let keymap = probe.keymap().ok()?;
        let held = newly_down(baseline, &keymap);
        if button1_down && !held.is_empty() {
            return Some(HeldState { button1_down, held_keycodes: held });
        }
        if started.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until all previously held keycodes and button 1 are released, or
/// `timeout` elapses.
pub fn wait_held_release(
    probe: &Probe,
    held_keycodes: &[u16],
    timeout: Duration,
) -> Option<ReleasedState> {
    let started = Instant::now();
    loop {
        let button1_down = probe.button1_down().ok()?;
        let keymap = probe.keymap().ok()?;
        let still_down: Vec<u16> = held_keycodes
            .iter()
            .copied()
            .filter(|code| {
                let index = usize::from(*code) / 8;
                let bit = *code % 8;
                index < 32 && keymap[index] & (1u8 << bit) != 0
            })
            .collect();
        if !button1_down && still_down.is_empty() {
            return Some(ReleasedState {
                button1_down,
                held_keycodes: still_down,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        if started.elapsed() >= timeout {
            return Some(ReleasedState {
                button1_down,
                held_keycodes: still_down,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ===========================================================================
// uinput: a real kernel keyboard device
// ===========================================================================

pub mod uinput {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int, c_ulong};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// `_IO('U', 1)`: create the device after `UI_DEV_SETUP`.
    const UI_DEV_CREATE: c_ulong = 0x5501;
    /// `_IO('U', 2)`.
    const UI_DEV_DESTROY: c_ulong = 0x5502;
    /// `_IOW('U', 3, struct uinput_setup)` (uinput_setup is 92 bytes).
    const UI_DEV_SETUP: c_ulong = 0x405c_5503;
    /// `_IOW('U', 100, int)`.
    const UI_SET_EVBIT: c_ulong = 0x4004_5564;
    /// `_IOW('U', 101, int)`.
    const UI_SET_KEYBIT: c_ulong = 0x4004_5565;

    const EV_KEY: u16 = 1;
    const EV_SYN: u16 = 0;
    const SYN_REPORT: u16 = 0;
    const BUS_VIRTUAL: u16 = 0x06;
    const KEY_F13: u16 = 183;
    const INPUT_EVENT_SIZE: usize = 24;

    #[repr(C)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }

    #[repr(C)]
    struct UinputSetup {
        id: InputId,
        name: [u8; 80],
        ff_effects_max: u32,
    }

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn mknod(path: *const c_char, mode: u32, dev: u64) -> c_int;
    }

    /// An error from the uinput or Xorg subsystem.
    pub struct Fault {
        reason: &'static str,
        detail: String,
    }

    impl Fault {
        pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
            Self { reason, detail: detail.into() }
        }

        pub fn reason(&self) -> &'static str {
            self.reason
        }

        pub fn describe(&self) -> String {
            format!("{}: {}", self.reason, self.detail)
        }
    }

    /// A kernel input device node discovered from sysfs.
    pub struct Node {
        pub sysfs: PathBuf,
        pub devnode: PathBuf,
        pub origin: &'static str,
    }

    /// A synthetic kernel keyboard created via `/dev/uinput`.
    pub struct Keyboard {
        file: std::fs::File,
        name: String,
        alive: bool,
    }

    impl Keyboard {
        /// Create a real kernel keyboard: a valid key set so the evdev
        /// driver classifies it as a keyboard, `BUS_VIRTUAL`, unique name.
        pub fn create(name: &str) -> Result<Self, Fault> {
            if std::mem::size_of::<usize>() != 8 {
                return Err(Fault::new(
                    "unsupported_word_size",
                    "the input_event/uinput ABI fixed by this harness is 64-bit timeval layout",
                ));
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/uinput")
                .map_err(|error| Fault::new("uinput_open", format!("/dev/uinput: {error}")))?;
            let fd = file.as_raw_fd();
            must_ioctl(fd, UI_SET_EVBIT, EV_KEY as c_int)?;
            must_ioctl(fd, UI_SET_EVBIT, EV_SYN as c_int)?;
            for code in keyboard_keys() {
                must_ioctl(fd, UI_SET_KEYBIT, code as c_int)?;
            }
            let mut setup = UinputSetup {
                id: InputId {
                    bustype: BUS_VIRTUAL,
                    vendor: 0x5348,
                    product: 0x0001,
                    version: 1,
                },
                name: [0u8; 80],
                ff_effects_max: 0,
            };
            let bytes = name.as_bytes();
            if bytes.len() >= setup.name.len() {
                return Err(Fault::new("uinput_name_too_long", name.to_string()));
            }
            setup.name[..bytes.len()].copy_from_slice(bytes);
            let setup_ptr = std::ptr::addr_of!(setup).cast::<std::ffi::c_void>();
            let setup_result =
                unsafe { ioctl(fd, UI_DEV_SETUP, setup_ptr) };
            if setup_result < 0 {
                return Err(Fault::new(
                    "uinput_dev_setup",
                    std::io::Error::last_os_error().to_string(),
                ));
            }
            must_ioctl(fd, UI_DEV_CREATE, 0)?;
            Ok(Self { file, name: name.to_string(), alive: true })
        }

        /// The kernel-owned identity of this device.
        pub fn await_node(&self, timeout: Duration) -> Result<Node, Fault> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(node) = self.find_node()? {
                    return Ok(node);
                }
                if Instant::now() >= deadline {
                    return Err(Fault::new(
                        "kernel_device_node_missing",
                        format!(
                            "no /sys/class/input/event* named {:?} appeared",
                            self.name
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn find_node(&self) -> Result<Option<Node>, Fault> {
            let entries = std::fs::read_dir("/sys/class/input").map_err(|error| {
                Fault::new("sysfs_input_missing", format!("/sys/class/input: {error}"))
            })?;
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().into_owned();
                if !file_name.starts_with("event") {
                    continue;
                }
                let sysfs = entry.path();
                let Ok(device_name) = std::fs::read_to_string(sysfs.join("device/name")) else {
                    continue;
                };
                if device_name.trim() != self.name {
                    continue;
                }
                let dev = std::fs::read_to_string(sysfs.join("dev")).map_err(|error| {
                    Fault::new("sysfs_dev_missing", format!("{}/dev: {error}", sysfs.display()))
                })?;
                let (major, minor) = parse_dev(&dev)?;
                let devnode = Path::new("/dev/input").join(&file_name);
                if devnode.exists() {
                    return Ok(Some(Node { sysfs, devnode, origin: "udev_node" }));
                }
                std::fs::create_dir_all("/dev/input").map_err(|error| {
                    Fault::new("dev_input_dir", format!("/dev/input: {error}"))
                })?;
                create_device_node(&devnode, major, minor)?;
                return Ok(Some(Node { sysfs, devnode, origin: "mknod_from_sysfs" }));
            }
            Ok(None)
        }

        /// Write one real `input_event` (plus its `SYN_REPORT`) through the
        /// kernel uinput device. Returns the bytes the kernel accepted.
        pub fn emit(&mut self, code: u16, value: i32) -> Result<Vec<usize>, Fault> {
            let key = encode_event(EV_KEY, code, value);
            let syn = encode_event(EV_SYN, SYN_REPORT, 0);
            let mut written = vec![];
            for record in [key, syn] {
                let count = self.file.write(&record).map_err(|error| {
                    Fault::new("uinput_write", format!("code {code} value {value}: {error}"))
                })?;
                if count != INPUT_EVENT_SIZE {
                    return Err(Fault::new(
                        "uinput_short_write",
                        format!("code {code}: wrote {count} of {INPUT_EVENT_SIZE}"),
                    ));
                }
                written.push(count);
            }
            Ok(written)
        }
    }

    impl Drop for Keyboard {
        fn drop(&mut self) {
            if !self.alive {
                return;
            }
            self.alive = false;
            let fd = self.file.as_raw_fd();
            unsafe {
                let _ = ioctl(fd, UI_DEV_DESTROY, 0);
            }
        }
    }

    fn keyboard_keys() -> Vec<u16> {
        let mut keys = vec![1u16 /* ESC */];
        keys.extend(2..=11u16);
        keys.extend(14..=53u16);
        keys.push(57);
        keys.push(58);
        keys.extend(59..=68u16);
        keys.push(87);
        keys.push(88);
        keys.push(KEY_F13);
        keys
    }

    fn must_ioctl(fd: c_int, request: c_ulong, value: c_int) -> Result<(), Fault> {
        let result = unsafe { ioctl(fd, request, value) };
        if result < 0 {
            return Err(Fault::new(
                "uinput_ioctl",
                format!(
                    "request 0x{request:x} value {value}: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        Ok(())
    }

    fn parse_dev(text: &str) -> Result<(u64, u64), Fault> {
        let (major, minor) = text.trim().split_once(':').ok_or_else(|| {
            Fault::new("sysfs_dev_format", format!("unexpected major:minor {text:?}"))
        })?;
        let major = major
            .parse()
            .map_err(|error| Fault::new("sysfs_dev_format", format!("{text:?}: {error}")))?;
        let minor = minor
            .parse()
            .map_err(|error| Fault::new("sysfs_dev_format", format!("{text:?}: {error}")))?;
        Ok((major, minor))
    }

    fn create_device_node(path: &Path, major: u64, minor: u64) -> Result<(), Fault> {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
            Fault::new("devnode_path", format!("{} is not a valid path", path.display()))
        })?;
        let result = unsafe { mknod(name.as_ptr(), 0o020000 | 0o600, makedev(major, minor)) };
        if result < 0 {
            return Err(Fault::new(
                "devnode_mknod",
                format!("{}: {}", path.display(), std::io::Error::last_os_error()),
            ));
        }
        Ok(())
    }

    fn makedev(major: u64, minor: u64) -> u64 {
        (minor & 0xff)
            | ((major & 0xfff) << 8)
            | ((minor & !0xffu64) << 12)
            | ((major & !0xfffu64) << 32)
    }

    fn encode_event(event_type: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_SIZE] {
        let mut record = [0u8; INPUT_EVENT_SIZE];
        record[16..18].copy_from_slice(&event_type.to_ne_bytes());
        record[18..20].copy_from_slice(&code.to_ne_bytes());
        record[20..24].copy_from_slice(&value.to_ne_bytes());
        record
    }
}

// ===========================================================================
// Owned, isolated Xorg server
// ===========================================================================

pub mod owned_xorg {
    use super::XORG_READY_TIMEOUT;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use x11rb::rust_connection::RustConnection;

    /// An error from the Xorg subsystem.
    pub struct Fault {
        reason: &'static str,
        detail: String,
    }

    impl Fault {
        pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
            Self { reason, detail: detail.into() }
        }

        pub fn reason(&self) -> &'static str {
            self.reason
        }

        pub fn describe(&self) -> String {
            format!("{}: {}", self.reason, self.detail)
        }
    }

    /// A running owned Xorg server bound to exactly one synthetic input
    /// device. Cleanup kills only this child and removes the lock/socket
    /// only while the lock still names the same PID.
    pub struct Server {
        child: Child,
        pub display: String,
        pub config: PathBuf,
        pub log: PathBuf,
        pid: u32,
    }

    impl Server {
        pub fn child_pid(&self) -> u32 {
            self.pid
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let lock = PathBuf::from(format!("/tmp/.X{}-lock", self.pid_display()));
            if std::fs::read_to_string(&lock)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                == Some(self.pid)
            {
                let _ = std::fs::remove_file(&lock);
            }
            let _ = std::fs::remove_file(socket_path(self.pid_display()));
        }
    }

    impl Server {
        fn pid_display(&self) -> u32 {
            self.display
                .trim_start_matches(':')
                .parse()
                .unwrap_or_default()
        }
    }

    /// Find a binary on `PATH`.
    pub fn find_binary(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let candidate = dir.join(name);
                candidate.is_file().then_some(candidate)
            })
        })
    }

    /// Find an Xorg driver module by stem name.
    pub fn find_driver(stem: &str) -> Option<PathBuf> {
        let mut dirs = vec![];
        if let Some(root) = std::env::var_os("SHENKO_XORG_MODULE_DIR") {
            let root = PathBuf::from(root);
            dirs.push(root.join("drivers"));
            dirs.push(root);
        }
        for base in [
            "/usr/lib/xorg/modules/drivers",
            "/usr/lib64/xorg/modules/drivers",
            "/usr/lib/x86_64-linux-gnu/xorg/modules/drivers",
            "/usr/lib/aarch64-linux-gnu/xorg/modules/drivers",
            "/usr/local/lib/xorg/modules/drivers",
        ] {
            dirs.push(PathBuf::from(base));
        }
        for dir in dirs {
            let candidate = dir.join(format!("{stem}_drv.so"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    /// Spawn an isolated Xorg server with exactly one synthetic keyboard
    /// and the `dummy` video driver.
    pub fn spawn(evdev_node: &Path, dir: &Path, binary: Option<&Path>) -> Result<Server, Fault> {
        let binary = binary.ok_or_else(|| Fault::new("xorg_missing", "no Xorg binary"))?;
        std::fs::create_dir_all(dir).map_err(|error| {
            Fault::new("xorg_dir", format!("{}: {error}", dir.display()))
        })?;
        let config = dir.join("xorg.conf");
        let configdir = dir.join("conf.d");
        std::fs::create_dir_all(&configdir).map_err(|error| {
            Fault::new("xorg_dir", format!("{}: {error}", configdir.display()))
        })?;
        std::fs::write(&config, config_text(evdev_node)).map_err(|error| {
            Fault::new("xorg_config", format!("{}: {error}", config.display()))
        })?;
        let mut attempts = vec![];
        let mut last_log: Option<PathBuf> = None;
        for extra in [
            vec!["-novtswitch"],
            vec!["-novtswitch", "-keeptty"],
            vec!["-keeptty"],
        ] {
            let display_num = match pick_display() {
                Some(number) => number,
                None => {
                    return Err(Fault::new(
                        "xorg_display_unavailable",
                        "no free :N display number with an unused lock file".to_string(),
                    ))
                }
            };
            let display = format!(":{display_num}");
            let log = dir.join(format!("xorg-{display_num}.log"));
            last_log = Some(log.clone());
            let mut command = Command::new(binary);
            command
                .arg(&display)
                .arg("-config")
                .arg(&config)
                .arg("-configdir")
                .arg(&configdir)
                .arg("-logfile")
                .arg(&log)
                .arg("-noreset")
                .arg("-nolisten")
                .arg("tcp");
            for flag in extra {
                command.arg(flag);
            }
            let child = command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| Fault::new("xorg_spawn", format!("{}: {error}", binary.display())))?;
            let pid = child.id();
            let mut server = Server {
                child,
                display,
                config: config.clone(),
                log: log.clone(),
                pid,
            };
            match wait_ready(&mut server, display_num, pid) {
                Ok(()) => return Ok(server),
                Err(error) => {
                    attempts.push(format!("{}: {}", server.display, error.describe()));
                    drop(server);
                }
            }
        }
        let tail = last_log
            .as_deref()
            .and_then(log_tail)
            .unwrap_or_else(|| "no Xorg log".into());
        Err(Fault::new(
            "xorg_start_failed",
            format!("every owned Xorg attempt failed: {}; log tail: {tail}", attempts.join(" | ")),
        ))
    }

    fn wait_ready(server: &mut Server, display_num: u32, pid: u32) -> Result<(), Fault> {
        let (display, lock, socket) = (
            server.display.clone(),
            PathBuf::from(format!("/tmp/.X{display_num}-lock")),
            socket_path(display_num),
        );
        let deadline = Instant::now() + XORG_READY_TIMEOUT;
        loop {
            match server.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(Fault::new(
                        "xorg_exited",
                        format!(
                            "server for {display} exited with {status}; log tail: {}",
                            log_tail(&server.log).unwrap_or_default()
                        ),
                    ))
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(Fault::new("xorg_wait", format!("{display}: {error}")));
                }
            }
            let owned_lock = std::fs::read_to_string(&lock)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                == Some(pid);
            if owned_lock
                && socket.exists()
                && RustConnection::connect(Some(display.as_str())).is_ok()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Fault::new(
                    "xorg_not_ready",
                    format!(
                        "{display} did not answer within {}s; log tail: {}",
                        XORG_READY_TIMEOUT.as_secs(),
                        log_tail(&server.log).unwrap_or_default()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn socket_path(display_num: u32) -> PathBuf {
        PathBuf::from(format!("/tmp/.X11-unix/X{display_num}"))
    }

    fn pick_display() -> Option<u32> {
        let seed = std::process::id().wrapping_mul(2_654_435_761);
        let mut seen = vec![];
        for attempt in 0..64u32 {
            let number = 200 + (seed.wrapping_add(attempt.wrapping_mul(7919)) % 500);
            if seen.contains(&number) {
                continue;
            }
            seen.push(number);
            if PathBuf::from(format!("/tmp/.X{number}-lock")).exists()
                || socket_path(number).exists()
            {
                continue;
            }
            return Some(number);
        }
        None
    }

    fn log_tail(path: &Path) -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        let tail: Vec<&str> = text.lines().rev().take(25).collect();
        Some(tail.into_iter().rev().collect::<Vec<_>>().join(" ⏎ "))
    }

    fn config_text(evdev_node: &Path) -> String {
        format!(
            r#"# Owned, isolated Xorg config written by the Shenko Computer Use
# native takeover proof. Automatic device addition is disabled, so no host
# or ambient input device is ever opened: the ONLY input device is the
# uinput keyboard created by this harness, addressed by its kernel node.
Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "DontVTSwitch" "true"
    Option "DontZap" "true"
EndSection

Section "Device"
    Identifier "ShenkoDummyDevice"
    Driver "dummy"
    VideoRam 65536
EndSection

Section "Monitor"
    Identifier "ShenkoDummyMonitor"
    HorizSync 5.0 - 200.0
    VertRefresh 5.0 - 200.0
    Modeline "1024x768" 65.00 1024 1048 1184 1344 768 771 777 806 -hsync -vsync
EndSection

Section "Screen"
    Identifier "ShenkoDummyScreen"
    Device "ShenkoDummyDevice"
    Monitor "ShenkoDummyMonitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1024x768"
    EndSubSection
EndSection

Section "InputDevice"
    Identifier "ShenkoUinputKeyboard"
    Driver "evdev"
    Option "Device" "{}"
    Option "GrabDevice" "false"
EndSection

Section "ServerLayout"
    Identifier "ShenkoLayout"
    Screen "ShenkoDummyScreen"
    InputDevice "ShenkoUinputKeyboard" "CoreKeyboard"
EndSection
"#,
            evdev_node.display()
        )
    }
}
