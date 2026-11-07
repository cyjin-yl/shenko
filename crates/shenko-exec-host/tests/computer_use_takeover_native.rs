//! Native Linux proof path: a real non-XTEST input device revokes an active
//! Computer Use write lease and releases machine-held input
//! (SPEC §24.1/§24.3).
//!
//! # What actually runs
//!
//! Every step below is a real mechanism; nothing is mocked, and the receipt
//! is written from what was observed:
//!
//! 1. This harness creates a genuine kernel input device by writing
//!    `uinput_setup` + `UI_DEV_CREATE` to `/dev/uinput` (a keyboard named
//!    `shenko-takeover-uinput-<uuid>`) and waits until the kernel publishes
//!    `eventN` in `/sys/class/input`, recording the kernel name and the
//!    `major:minor` identity of the node.
//! 2. It spawns an OWNED Xorg server (real `dummy` video driver + the real
//!    `evdev` input driver bound to exactly that kernel event node, with
//!    `AutoAddDevices`/`AutoEnableDevices` disabled and a private
//!    `-configdir`), so no ambient desktop or host input device is touched.
//! 3. It opens the real `X11Provider` — the provider the CLI registers —
//!    acquires the single write lease of that display domain, captures a
//!    real observation, builds and seals the observation record through the
//!    same Store/CAS contract `ComputerUseBackend::observe` uses (provider,
//!    domain, display, frame/revision, capture epoch, region, viewport, DPI,
//!    layout fingerprint and pixel hash are the provider's real values; the
//!    sealed record digest is the durable observation id for this store),
//!    and injects real XTEST input that leaves one pointer button and one
//!    key HELD.
//! 4. It writes a `KEY_F13` press/release into the uinput device. The kernel
//!    delivers it to the evdev node, Xorg's evdev driver delivers it to its
//!    slave keyboard, and the server emits an XInput2 raw key event whose
//!    `sourceid` is that slave device.
//! 5. The provider's domain watcher (its own X connection, XI2 raw
//!    selection) sees a non-XTEST source, drops the lease holder, bumps the
//!    domain epoch and releases the machine-held button/key.
//! 6. This harness records — from sources INDEPENDENT of the provider's own
//!    watcher — that (a) the external event really came from the kernel
//!    device (its own XI2 raw selection on a second connection, plus its own
//!    read of the kernel evdev node), (b) the held input is up again (its
//!    own `XQueryPointer`/`XQueryKeymap`), and (c) the old lease token can
//!    no longer write (`no_lease`, then `lease_revoked` after a new writer
//!    acquired).
//!
//! # Honesty boundaries
//!
//! * The device is a *synthetic kernel device*. This proves the non-XTEST
//!   external-input revocation mechanism. It is NOT a claim that a physical
//!   human pressed a key: the receipt carries `human_intent_claim: false`
//!   plus `mechanism`.
//! * A lease can never stop a human or an unmanaged X client from typing; it
//!   only invalidates this product's stale controller epoch. That — and the
//!   held-input release fence — is what is measured.
//! * The product task ledger (`ComputerUseSessionHandler` … `drive`) is not
//!   driven here because its fixture lives in the CLI test binary. This
//!   harness exercises the same provider API those handlers call and records
//!   the same typed `DesktopError` reasons the product normalizes
//!   (`no_lease`, `lease_revoked`). The CLI fixture owns the durable
//!   operation receipt for this trigger.
//! * The existing CLI Xvfb suite can NOT prove this case: Xvfb exposes only
//!   XTEST devices, so every raw event there is filtered as self-injection.
//!   That gap is exactly what this harness closes.
//!
//! # Prerequisites (parent-owned; a missing capability ⇒ `inconclusive`)
//!
//! * Linux (x86_64/aarch64), as a user allowed to open `/dev/uinput`
//!   read/write and to start Xorg. In the development container that
//!   normally means `--privileged`, or the devices `/dev/uinput`,
//!   `/dev/input`, `/dev/tty0`, `/dev/console` plus `CAP_SYS_ADMIN`.
//! * Packages: `xserver-xorg-core`, `xserver-xorg-video-dummy`,
//!   `xserver-xorg-input-evdev`, `xkb-data`.
//! * `/dev/input/eventN` is normally created by udev. When udev does not run
//!   in the container the harness creates the node with `mknod` from the
//!   kernel's own `/sys/class/input/eventN/dev` and records
//!   `node_origin: "mknod_from_sysfs"`.
//!
//! # Isolation and cleanup
//!
//! * The X server is spawned by this harness on a display number whose lock
//!   file and socket are free, with the PID-checked `/tmp/.X<N>-lock` as
//!   ownership proof. Teardown kills only that child and removes the lock
//!   and socket only while the lock still names the same PID.
//! * `AutoAddDevices`/`AutoEnableDevices` are disabled and the configuration
//!   comes from a private `-configdir`, so no host input device is opened.
//! * The uinput device is a NEW keyboard created here and destroyed with
//!   `UI_DEV_DESTROY`; existing devices are never opened for write. `KEY_F13`
//!   has no default desktop binding, so even on a shared kernel the only
//!   visible effect is a function-key press nobody binds.
//!
//! # Run (parent-owned; the author never executes this)
//!
//! ```text
//! cargo test -p shenko-exec-host --test computer_use_takeover_native \
//!     -- --ignored --nocapture
//! ```
//!
//! A missing prerequisite or a lost capability fails RED with
//! `INCONCLUSIVE (no pass)` and a retained `receipt.json`; it can never
//! return success. The evidence root defaults to
//! `<project>/build/computer-use-takeover-native`, resolved by the `SPEC.md`
//! marker (so it never lands inside the source worktree);
//! `SHENKO_TAKEOVER_EVIDENCE_DIR` overrides it. Each run retains its own
//! directory with `receipt.json`, the observation CAS (`store/`) and the
//! Xorg config/log (`xorg/`).

// The whole harness is a Linux mechanism (uinput/evdev/Xorg); the crate-level
// gate keeps other hosts compiling an empty test target instead of an
// unbuildable one. A host that is not Linux therefore has no proof and no
// green result to point at.
#![cfg(target_os = "linux")]

use shenko_exec_host::{
    DesktopAction, DesktopCoordinateTransform, DesktopError, DesktopInjectRequest, DesktopLeaseOp,
    DesktopObservation, DesktopOpenRequest, DesktopPress, DesktopProvider, DesktopSession,
    DesktopSessionHandle, HumanInputFanout, HumanInputObserver, X11Provider, encode_png,
    hex_digest,
};
use shenko_exec_host::takeover_proof::{
    self, owned_xorg, uinput, DispatchRecorder, EvdevReader, Monitor, Probe,
    HeldState, ReleasedState, RawEventFacts, KernelEventFacts,
    EV_KEY, KEY_F13_EVDEV,
    EVENT_TIMEOUT, STATE_TIMEOUT, NODE_TIMEOUT,
    down_keycodes, newly_down, wait_held_state, wait_held_release,
};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct Prerequisite {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(serde::Serialize)]
struct ProviderFacts {
    provider_id: String,
    capabilities: serde_json::Value,
}

#[derive(serde::Serialize)]
struct SessionFacts {
    session_id: String,
    provider: String,
    session_kind: String,
    domain_id: String,
    display: String,
    display_id: String,
    root_window: u64,
    screen_num: u64,
    width_px: u64,
    height_px: u64,
    dpi_x: f64,
    dpi_y: f64,
    depth: u64,
    xtest_version: Option<String>,
    xinput2_version: Option<String>,
    xtest_source_device_ids: Vec<u16>,
    external_takeover_detection: bool,
}

#[derive(serde::Serialize)]
struct KernelFacts {
    device_name: String,
    sysfs_dir: String,
    evdev_node: String,
    node_origin: &'static str,
    uinput_bytes_written: Vec<usize>,
    kernel_events: Vec<KernelEventFacts>,
    kernel_events_elapsed_ms: Option<u64>,
}

#[derive(serde::Serialize)]
struct ExternalFacts {
    independent_x_connection: &'static str,
    selection: &'static str,
    device_id: u16,
    device_name: String,
    xtest_source_device_ids_from_monitor: Vec<u16>,
    raw_events: Vec<RawEventFacts>,
    matched_raw_key_press: bool,
    raw_key_release_observed: bool,
    elapsed_ms: Option<u64>,
}

#[derive(serde::Serialize)]
struct ObservationFacts {
    observation_id: String,
    frame_id: u64,
    revision: u64,
    lease_epoch_at_capture: u64,
    region: [i32; 4],
    viewport: [u16; 2],
    layout_fingerprint: String,
    pixel_hash: String,
    raw_pixels_digest: String,
    png_preview_digest: String,
    captured_at_ms: i64,
}

#[derive(serde::Serialize)]
struct ReceiptFacts {
    index: usize,
    kind: String,
    transport: &'static str,
    observed_effect: Option<String>,
}

#[derive(serde::Serialize)]
struct HeldFacts {
    actions: Vec<String>,
    sent_events: usize,
    receipts: Vec<ReceiptFacts>,
    postcondition_verified: bool,
    button1_down_observed: bool,
    held_keycodes: Vec<u16>,
    pointer_before: [i16; 2],
}

#[derive(serde::Serialize)]
struct DispatchFacts {
    domains: Vec<String>,
    session_domain_dispatched: bool,
    elapsed_ms: Option<u64>,
}

#[derive(serde::Serialize)]
struct RevocationFacts {
    epoch_before: u64,
    epoch_after_new_lease: u64,
    epoch_delta: u64,
    /// Bounded pause after the external press+release so the single watcher
    /// thread has drained both raw events before this harness acquires a new
    /// lease. Each external event legitimately bumps the epoch once, so the
    /// assertion is `delta >= 1`; the exact value is recorded, never guessed.
    watcher_settle_ms: u64,
}

#[derive(serde::Serialize)]
struct ReleaseFacts {
    button1_up_after: bool,
    keycodes_up_after: Vec<u16>,
    elapsed_ms: Option<u64>,
    snapshot_source: &'static str,
}

#[derive(serde::Serialize)]
struct RefusalFacts {
    phase: &'static str,
    lease_token: String,
    observation_id: String,
    actions: Vec<String>,
    outcome: &'static str,
    reason: Option<String>,
    detail: Option<String>,
    sent_events: Option<usize>,
    pointer_before: [i16; 2],
    pointer_after: [i16; 2],
}

#[derive(serde::Serialize)]
struct NewLeaseFacts {
    result: String,
    epoch: u64,
    lease_token: String,
    fresh_observation_id: String,
    fresh_epoch_at_capture: u64,
    injected_sent_events: usize,
    postcondition_verified: bool,
    pointer_after: [i16; 2],
}

#[derive(serde::Serialize)]
struct LeaseProbeFacts {
    /// "accepted" = the provider admitted the live lease and the bound
    /// observation at the instant before the external device event.
    provider_result: &'static str,
    sent_events: usize,
    observation_id: String,
    note: &'static str,
}

#[derive(serde::Serialize)]
struct CloseFacts {
    held_released: usize,
    lease_revoked: bool,
}

#[derive(serde::Serialize)]
struct Evidence {
    schema_version: u32,
    kind: &'static str,
    mechanism: &'static str,
    claim_scope: &'static str,
    human_intent_claim: bool,
    result: String,
    failure: Option<String>,
    started_at: String,
    finished_at: String,
    run_dir: String,
    prerequisites: Vec<Prerequisite>,
    provider: Option<ProviderFacts>,
    session: Option<SessionFacts>,
    kernel_device: Option<KernelFacts>,
    observation_before: Option<ObservationFacts>,
    held_input: Option<HeldFacts>,
    lease_active_before_external_event: Option<LeaseProbeFacts>,
    external_device_event: Option<ExternalFacts>,
    product_dispatch: Option<DispatchFacts>,
    revocation: Option<RevocationFacts>,
    held_release: Option<ReleaseFacts>,
    old_lease_refusals: Vec<RefusalFacts>,
    new_lease: Option<NewLeaseFacts>,
    session_close: Option<CloseFacts>,
    artifacts: Vec<String>,
    notes: Vec<String>,
}

impl Evidence {
    fn new(run_dir: &Path) -> Self {
        Self {
            schema_version: 1,
            kind: "shenko.computer_use.native_takeover/v1",
            mechanism:
                "synthetic kernel uinput keyboard -> kernel evdev -> owned Xorg evdev driver -> \
                 XInput2 raw key event -> provider lease revocation + held-input release",
            claim_scope:
                "proves the non-XTEST external-input revocation mechanism and the held-input \
                 release fence on a real X server; does not claim a physical human key press, a \
                 Windows/macOS path, or the durable product task receipt",
            human_intent_claim: false,
            result: "unresolved".into(),
            failure: None,
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: String::new(),
            run_dir: run_dir.display().to_string(),
            prerequisites: vec![],
            provider: None,
            session: None,
            kernel_device: None,
            observation_before: None,
            held_input: None,
            lease_active_before_external_event: None,
            external_device_event: None,
            product_dispatch: None,
            revocation: None,
            held_release: None,
            old_lease_refusals: vec![],
            new_lease: None,
            session_close: None,
            artifacts: vec![],
            notes: vec![
                "The provider rejections recorded here are the same DesktopError reasons the \
                 product layer maps to result:rejected; the CLI fixture owns the durable \
                 operation receipt for this trigger."
                    .into(),
            ],
        }
    }
}

/// Why a bounded run stopped without completing its assertions.
enum Stop {
    /// The environment cannot provide a real mechanism (never a pass).
    Inconclusive(String),
    /// The mechanism ran and a real observation contradicted the contract.
    Failed(String),
}

fn failed<T>(detail: impl Into<String>) -> Result<T, Stop> {
    Err(Stop::Failed(detail.into()))
}

/// Record that a real prerequisite was met.
fn met(ev: &mut Evidence, name: &'static str, detail: impl Into<String>) {
    ev.prerequisites.push(Prerequisite { name, ok: true, detail: detail.into() });
}

/// Record a missing capability and return the stop it forces. This is the
/// ONLY way a run stops as inconclusive: the receipt names the prerequisite.
fn unmet(ev: &mut Evidence, name: &'static str, detail: impl Into<String>) -> Stop {
    let detail = detail.into();
    ev.prerequisites.push(Prerequisite { name, ok: false, detail: detail.clone() });
    Stop::Inconclusive(format!("{name}: {detail}"))
}

fn map_provider<T>(what: &str, result: Result<T, DesktopError>) -> Result<T, Stop> {
    result.map_err(|error| match error {
        DesktopError::Unavailable { reason, detail } => {
            Stop::Inconclusive(format!("{what}: unavailable {reason} {detail}"))
        }
        other => Stop::Failed(format!("{what}: {other}")),
    })
}

// ---------------------------------------------------------------------------
// Test entry
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns an owned Xorg on an owned /dev/uinput device; parent runs it explicitly (see module docs)"]
fn uinput_external_key_revokes_computer_use_lease_and_releases_held_input() {
    let run_dir = evidence_root().join(format!(
        "takeover-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        EntityId::new()
    ));
    std::fs::create_dir_all(&run_dir).unwrap_or_else(|error| {
        panic!("cannot create evidence directory {}: {error}", run_dir.display())
    });
    let mut ev = Evidence::new(&run_dir);
    match run(&mut ev, &run_dir) {
        Ok(()) => ev.result = "proven".into(),
        Err(Stop::Inconclusive(detail)) => {
            ev.result = "inconclusive".into();
            ev.failure = Some(detail);
        }
        Err(Stop::Failed(detail)) => {
            ev.result = "failed".into();
            ev.failure = Some(detail);
        }
    }
    ev.finished_at = chrono::Utc::now().to_rfc3339();
    let receipt = write_evidence(&run_dir, &ev);
    let result = ev.result.clone();
    eprintln!("computer.use native takeover evidence: {}", receipt.display());
    for entry in &ev.prerequisites {
        eprintln!("  prerequisite {} = {} ({})", entry.name, entry.ok, entry.detail);
    }
    match result.as_str() {
        "proven" => {
            eprintln!(
                "PROVEN: a non-XTEST kernel device event revoked the lease, released held input \
                 and fenced the old lease token (synthetic kernel device, not human intent)"
            );
        }
        "inconclusive" => panic!(
            "INCONCLUSIVE (no pass): {} — evidence {}",
            ev.failure.unwrap_or_default(),
            receipt.display()
        ),
        _ => panic!(
            "FAILED: {} — evidence {}",
            ev.failure.unwrap_or_default(),
            receipt.display()
        ),
    }
}

fn evidence_root() -> PathBuf {
    if let Ok(dir) = std::env::var("SHENKO_TAKEOVER_EVIDENCE_DIR") {
        return PathBuf::from(dir);
    }
    // Resolve the project root by the SPEC.md marker, not by a fixed number
    // of `..` steps: the crate is built both inside `worktrees/<name>` and in
    // a plain clone. Evidence stays outside the source tree either way.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for depth in [4usize, 2] {
        let mut root = manifest.to_path_buf();
        for _ in 0..depth {
            root.pop();
        }
        if root.join("SPEC.md").is_file() {
            return root.join("build/computer-use-takeover-native");
        }
    }
    manifest.join("../../build/computer-use-takeover-native")
}

fn write_evidence(run_dir: &Path, ev: &Evidence) -> PathBuf {
    let path = run_dir.join("receipt.json");
    match serde_json::to_vec_pretty(ev) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(&path, bytes) {
                eprintln!("could not write evidence to {}: {error}", path.display());
            }
        }
        Err(error) => eprintln!("could not serialize evidence: {error}"),
    }
    path
}

// ---------------------------------------------------------------------------
// The bounded run
// ---------------------------------------------------------------------------

fn run(ev: &mut Evidence, run_dir: &Path) -> Result<(), Stop> {
    // Declaration order matters for teardown (drops run in reverse): the
    // uinput device and the owned X server outlive every client/provider.
    let uinput_name = format!("shenko-takeover-uinput-{}", EntityId::new());
    let mut keyboard = match uinput::Keyboard::create(&uinput_name) {
        Ok(keyboard) => keyboard,
        Err(fault) => return Err(unmet(ev, "linux_uinput_device", fault.describe())),
    };
    met(ev, "linux_uinput_device", format!("/dev/uinput accepted {uinput_name}"));

    let node = match keyboard.await_node(NODE_TIMEOUT) {
        Ok(node) => node,
        Err(fault) => return Err(unmet(ev, "kernel_input_device_node", fault.describe())),
    };
    let mut kernel = KernelFacts {
        device_name: uinput_name.clone(),
        sysfs_dir: node.sysfs.display().to_string(),
        evdev_node: node.devnode.display().to_string(),
        node_origin: node.origin,
        uinput_bytes_written: vec![],
        kernel_events: vec![],
        kernel_events_elapsed_ms: None,
    };
    met(
        ev,
        "kernel_input_device_node",
        format!("{} ({})", node.devnode.display(), node.origin),
    );

    // A real Store/CAS is opened for the observation record only. The record
    // is sealed exactly like `ComputerUseBackend::observe` seals it, so its
    // digest is a real content-addressed observation id in this store; no
    // task ledger, genesis or subject step is pretended to have run.
    std::fs::create_dir_all(run_dir.join("store"))
        .map_err(|error| Stop::Failed(format!("create store dir: {error}")))?;
    let store = Store::open(
        run_dir.join("store/shenko.db"),
        run_dir.join("store/cas"),
    )
    .map_err(|error| Stop::Failed(format!("open fixture store: {error}")))?;

    let Some(binary_path) = owned_xorg::find_binary("Xorg") else {
        return Err(unmet(
            ev,
            "xorg_binary",
            "no `Xorg` in PATH (install xserver-xorg-core)",
        ));
    };
    met(ev, "xorg_binary", binary_path.display().to_string());
    let dummy = owned_xorg::find_driver("dummy");
    let evdev = owned_xorg::find_driver("evdev");
    let drivers_detail = format!(
        "dummy={} evdev={} (install xserver-xorg-video-dummy + xserver-xorg-input-evdev)",
        dummy
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "missing".into()),
        evdev
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "missing".into())
    );
    if dummy.is_none() || evdev.is_none() {
        return Err(unmet(ev, "xorg_drivers", drivers_detail));
    }
    met(ev, "xorg_drivers", drivers_detail);

    let xorg_dir = run_dir.join("xorg");
    let binary = Some(binary_path.as_path());
    let server = match owned_xorg::spawn(&node.devnode, &xorg_dir, binary) {
        Ok(server) => server,
        Err(fault) => {
            ev.artifacts.push(format!("{}/", xorg_dir.display()));
            return Err(unmet(ev, "xorg_owned_server_ready", fault.describe()));
        }
    };
    ev.artifacts.push(server.config.display().to_string());
    ev.artifacts.push(server.log.display().to_string());
    met(
        ev,
        "xorg_owned_server_ready",
        format!("display {} pid {}", server.display, server.child_pid()),
    );

    // An independent XI2 raw-event client: its only job is to observe what
    // the server delivers, never to inject or to call the provider. The
    // input driver attaches asynchronously to the socket being ready, so a
    // device that is not there yet is retried inside a bounded window.
    let monitor = {
        let deadline = Instant::now() + STATE_TIMEOUT;
        loop {
            match Monitor::open(&server.display, &uinput_name) {
                Ok(monitor) => break monitor,
                Err(fault)
                    if fault.reason() == "xorg_input_device_not_admitted"
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fault) => {
                    return Err(unmet(ev, "xorg_input_device_admitted", fault.describe()))
                }
            }
        }
    };
    met(
        ev,
        "xorg_input_device_admitted",
        format!(
            "device {} = {:?} in the XInput2 hierarchy",
            monitor.device_id, monitor.device_name
        ),
    );
    met(
        ev,
        "xinput2_raw_client",
        format!(
            "XI {}.{} selected XIAllMasterDevices raw events on an independent connection",
            monitor.major, monitor.minor
        ),
    );
    if !(monitor.major > 2 || (monitor.major == 2 && monitor.minor >= 1)) {
        return Err(unmet(
            ev,
            "xinput2_raw_sourceids",
            format!(
                "server negotiated XI {}.{} (< 2.1): raw sourceid undefined",
                monitor.major, monitor.minor
            ),
        ));
    }
    let device_classification = format!(
        "device id {} {:?}; XTEST ids {:?}",
        monitor.device_id, monitor.device_name, monitor.xtest_ids
    );
    if monitor.device_name.to_ascii_uppercase().contains("XTEST")
        || monitor.xtest_ids.contains(&monitor.device_id)
    {
        return Err(unmet(ev, "non_xtest_kernel_device", device_classification));
    }
    met(ev, "non_xtest_kernel_device", device_classification);

    // Independent state probe (`XQueryPointer`/`XQueryKeymap`) on its own
    // connection, so no provider call is trusted for the X server's state.
    let probe = match Probe::connect(&server.display) {
        Ok(probe) => probe,
        Err(fault) => return Err(unmet(ev, "x11_state_probe", fault.describe())),
    };
    met(ev, "x11_state_probe", "own RustConnection for XQueryPointer/XQueryKeymap");

    // A reader of the kernel event stream: the device is NOT grabbed by
    // Xorg (`GrabDevice false`), so both the server and this harness observe
    // the very same kernel events.
    let mut kernel_reader = match EvdevReader::open(&node.devnode) {
        Ok(reader) => reader,
        Err(fault) => return Err(unmet(ev, "kernel_evdev_reader", fault.describe())),
    };
    met(
        ev,
        "kernel_evdev_reader",
        format!("independent read fd on {}", node.devnode.display()),
    );

    // Product-level dispatch observation: the provider's real
    // `HumanInputFanout` is what tells the browser provider to drop its
    // agent lease. This observer only records the dispatch; it never
    // triggers or fakes a revocation.
    let fanout = Arc::new(HumanInputFanout::new());
    let dispatch = Arc::new(DispatchRecorder::default());
    let observer: Arc<dyn HumanInputObserver> = dispatch.clone();
    fanout.register(observer);
    let provider = X11Provider::with_fanout(Arc::clone(&fanout));
    ev.provider = Some(ProviderFacts {
        provider_id: provider.provider_id().into(),
        capabilities: serde_json::to_value(provider.capabilities()).unwrap_or_default(),
    });

    let instance = EntityId::new();
    let request = DesktopOpenRequest { display: Some(server.display.clone()), screen: None };
    let handle = map_provider("open x11 session", provider.open(&request, instance))?;
    let display = handle.session.display();
    let session = session_facts(&handle, &display);
    // A provider that cannot negotiate XI2 raw detection here is a real
    // product gap, not an environment gap: this harness proved above that
    // the server speaks XI >= 2.1 and enumerates the kernel device.
    if !session.external_takeover_detection {
        return failed(format!(
            "provider reports external_takeover_detection=false while the server negotiated \
             XI {}.{} and the raw client selected XIAllMasterDevices",
            monitor.major, monitor.minor
        ));
    }
    if session.xtest_version.is_none() {
        return failed("provider could not probe the XTEST extension on a real Xorg server");
    }
    if session.xtest_source_device_ids.is_empty() {
        return failed(
            "provider enumerated no XTEST source devices on a real Xorg server; its own \
             self-injection filter would have no truth to compare against",
        );
    }
    if session.xtest_source_device_ids.contains(&monitor.device_id) {
        return failed(format!(
            "the kernel device {:?} was classified as an XTEST source by the provider; a genuine \
             external event would be filtered as self-injection",
            monitor.device_name
        ));
    }
    ev.session = Some(session);

    let lease = map_provider("acquire lease", provider.lease_op(
        &handle,
        DesktopLeaseOp::Acquire,
        None,
        instance,
    ))?;
    let Some(lease_token) = lease.lease_token else {
        return failed("lease acquire returned no token");
    };
    if lease.result != "acquired" {
        return failed(format!("fresh domain lease acquire returned {:?}", lease.result));
    }
    let epoch_before = lease.epoch;

    let frame = map_provider("observe", provider.observe(&handle, None))?;
    if frame.lease_epoch != epoch_before {
        return failed(format!(
            "observation captured at epoch {} while the live lease epoch is {}",
            frame.lease_epoch, epoch_before
        ));
    }
    let (observation, observation_id) = build_observation(&store, &handle, &frame)?;
    ev.observation_before = Some(observation_facts(
        &observation,
        &observation_id,
        &frame.raw_pixels,
        &observation.png_preview_ref.digest.0,
    ));

    // Held input: independently record the X server state before/after the
    // provider's own injection; the provider is never the source of truth
    // for what the server holds.
    let keymap_before = probe
        .keymap()
        .map_err(|fault| Stop::Failed(format!("baseline XQueryKeymap: {}", fault.describe())))?;
    let pointer_before = probe
        .pointer()
        .map_err(|fault| Stop::Failed(format!("baseline XQueryPointer: {}", fault.describe())))?;

    let held_actions = vec![
        DesktopAction::Button { button: 1, press: DesktopPress::Down },
        DesktopAction::Key { key: "a".into(), press: Some(DesktopPress::Down) },
    ];
    let held_outcome = map_provider(
        "inject held input",
        provider.inject(
            &handle,
            DesktopInjectRequest {
                lease_token,
                actions: held_actions.clone(),
                observation: observation.clone(),
                require_pixel_fresh: false,
                checkpoint: Box::new(|| Ok(())),
            },
            EntityId::new(),
        ),
    )?;
    if held_outcome.sent_events != 2 || held_outcome.receipts.len() != 2 {
        return failed(format!(
            "held-input batch delivered {} events in {} receipts (expected 2/2)",
            held_outcome.sent_events,
            held_outcome.receipts.len()
        ));
    }
    if let Some(reason) = &held_outcome.stopped_reason {
        return failed(format!("held-input batch stopped early: {reason}"));
    }
    let held_state = wait_held_state(&probe, &keymap_before, STATE_TIMEOUT).ok_or_else(|| {
        Stop::Failed("the X server never showed the injected button/key as held".into())
    })?;
    ev.held_input = Some(HeldFacts {
        actions: held_actions.iter().map(action_label).collect(),
        sent_events: held_outcome.sent_events,
        receipts: held_outcome
            .receipts
            .iter()
            .map(|receipt| ReceiptFacts {
                index: receipt.index,
                kind: receipt.kind.clone(),
                transport: receipt.transport,
                observed_effect: receipt.observed_effect.clone(),
            })
            .collect(),
        postcondition_verified: held_outcome.postcondition_verified,
        button1_down_observed: held_state.button1_down,
        held_keycodes: held_state.held_keycodes.clone(),
        pointer_before,
    });

    // The genuine external device event: a kernel key press on a device the
    // provider itself must classify as non-XTEST.
    // Before it lands, prove the lease is the LIVE writer: an empty action
    // batch runs the provider's real lease + observation verification and,
    // by construction, injects nothing.
    let active_probe = map_provider(
        "probe live lease before the external device event",
        provider.inject(
            &handle,
            DesktopInjectRequest {
                lease_token,
                actions: vec![],
                observation: observation.clone(),
                require_pixel_fresh: false,
                checkpoint: Box::new(|| Ok(())),
            },
            EntityId::new(),
        ),
    )?;
    if active_probe.sent_events != 0 {
        return failed(format!(
            "the live-lease probe carried no actions but reported {} sent events",
            active_probe.sent_events
        ));
    }
    ev.lease_active_before_external_event = Some(LeaseProbeFacts {
        provider_result: "accepted",
        sent_events: active_probe.sent_events,
        observation_id: observation_id.clone(),
        note: "empty action batch: the provider verified the live lease epoch and the bound \
               observation, and injected no event",
    });

    let external_started = Instant::now();
    let press = keyboard
        .emit(KEY_F13_EVDEV, 1)
        .map_err(|fault| Stop::Failed(format!("uinput KEY_F13 press: {}", fault.describe())))?;
    std::thread::sleep(Duration::from_millis(50));
    let release = keyboard
        .emit(KEY_F13_EVDEV, 0)
        .map_err(|fault| Stop::Failed(format!("uinput KEY_F13 release: {}", fault.describe())))?;
    kernel.uinput_bytes_written = press.into_iter().chain(release).collect();

    let mut kernel_events = vec![];
    let mut kernel_press_elapsed = None;
    let kernel_deadline = Instant::now();
    while kernel_deadline.elapsed() < EVENT_TIMEOUT {
        kernel_events.extend(kernel_reader.drain());
        if kernel_events
            .iter()
            .any(|event| event.event_type == EV_KEY && event.code == KEY_F13_EVDEV && event.value == 1)
        {
            kernel_press_elapsed = Some(kernel_deadline.elapsed().as_millis() as u64);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    kernel.kernel_events = kernel_events;
    kernel.kernel_events_elapsed_ms = kernel_press_elapsed;
    if kernel_press_elapsed.is_none() {
        ev.kernel_device = Some(kernel);
        return failed(format!(
            "the kernel never reported KEY_F13 on {}; the external device event was not real",
            node.devnode.display()
        ));
    }
    ev.kernel_device = Some(kernel);

    // Both kernel records reach the server as two raw key events; collect
    // both on the independent connection so the evidence names the complete
    // external event, not just its first half.
    let mut raw_events: Vec<RawEventFacts> = vec![];
    let mut press_elapsed: Option<u64> = None;
    let mut release_seen = false;
    let raw_deadline = Instant::now();
    while raw_deadline.elapsed() < EVENT_TIMEOUT {
        monitor.drain_into(&mut raw_events);
        if press_elapsed.is_none()
            && raw_events
                .iter()
                .any(|event| event.sourceid == monitor.device_id && event.kind == "raw_key_press")
        {
            press_elapsed = Some(raw_deadline.elapsed().as_millis() as u64);
        }
        release_seen = raw_events
            .iter()
            .any(|event| event.sourceid == monitor.device_id && event.kind == "raw_key_release");
        if press_elapsed.is_some() && release_seen {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ev.external_device_event = Some(ExternalFacts {
        independent_x_connection: "separate RustConnection",
        selection: "XI_RawKey*/XI_RawButton*/XI_RawMotion on XIAllMasterDevices (root window)",
        device_id: monitor.device_id,
        device_name: monitor.device_name.clone(),
        xtest_source_device_ids_from_monitor: monitor.xtest_ids.clone(),
        raw_events,
        matched_raw_key_press: press_elapsed.is_some(),
        raw_key_release_observed: release_seen,
        elapsed_ms: press_elapsed,
    });
    if press_elapsed.is_none() {
        return failed(format!(
            "no XInput2 raw key press with sourceid {} ({:?}) reached an independent client",
            monitor.device_id, monitor.device_name
        ));
    }

    // The provider's watcher polls its own connection on a bounded interval;
    // the press and the release are two real external events, so let the
    // single watcher thread drain both before a new writer is admitted.
    let watcher_settle_ms = 150u64;
    std::thread::sleep(Duration::from_millis(watcher_settle_ms));

    // The provider's detection site must have dispatched this domain.
    let domains = dispatch.wait(EVENT_TIMEOUT);
    let session_domain = handle.session.domain_id();
    ev.product_dispatch = Some(DispatchFacts {
        session_domain_dispatched: domains.iter().any(|domain| domain == &session_domain),
        domains: domains.clone(),
        elapsed_ms: Some(external_started.elapsed().as_millis() as u64),
    });

    // The held input must be up again, observed by this harness's own X
    // connection, not by a provider report.
    let released = wait_held_release(&probe, &held_state.held_keycodes, STATE_TIMEOUT);
    ev.held_release = Some(ReleaseFacts {
        button1_up_after: released.as_ref().is_some_and(|state| !state.button1_down),
        keycodes_up_after: released
            .as_ref()
            .map(|state| state.held_keycodes.clone())
            .unwrap_or_default(),
        elapsed_ms: released.as_ref().map(|state| state.elapsed_ms),
        snapshot_source: "independent XQueryPointer/XQueryKeymap on a separate connection",
    });

    // Refusal 1: while no writer exists (the watcher dropped the holder),
    // the OLD token must not write, even with the observation it was bound
    // to. Nothing is sent on rejection, so the pointer must not move.
    let pointer_before_refusal = probe
        .pointer()
        .map_err(|fault| Stop::Failed(format!("XQueryPointer before refusal: {}", fault.describe())))?;
    let first_attempt = provider.inject(
        &handle,
        DesktopInjectRequest {
            lease_token,
            actions: vec![DesktopAction::PointerMove { x: 7, y: 7 }],
            observation: observation.clone(),
            require_pixel_fresh: false,
            checkpoint: Box::new(|| Ok(())),
        },
        EntityId::new(),
    );
    let pointer_after_refusal = probe
        .pointer()
        .map_err(|fault| Stop::Failed(format!("XQueryPointer after refusal: {}", fault.describe())))?;
    let first_refusal = refusal_facts(
        "after_external_revocation_before_new_lease",
        lease_token,
        &observation_id,
        first_attempt,
        pointer_before_refusal,
        pointer_after_refusal,
    );
    let first_reason = first_refusal.reason.clone();
    let first_outcome = first_refusal.outcome;
    ev.old_lease_refusals.push(first_refusal);
    if first_outcome != "rejected" {
        return failed(
            "the external device event did not revoke the lease: old-lease input was accepted",
        );
    }
    if first_reason.as_deref() != Some("no_lease") {
        return failed(format!(
            "old-lease input was refused with {:?} instead of no_lease",
            first_reason
        ));
    }
    if pointer_before_refusal != pointer_after_refusal {
        return failed(format!(
            "pointer moved from {pointer_before_refusal:?} to {pointer_after_refusal:?} on a \
             rejected batch"
        ));
    }
    if !released.as_ref().is_some_and(|state| !state.button1_down) {
        return failed("the machine-held button was still down after the revocation");
    }
    if released
        .as_ref()
        .is_none_or(|state| !state.held_keycodes.is_empty())
    {
        return failed("the machine-held key was still down after the revocation");
    }
    if !ev
        .product_dispatch
        .as_ref()
        .is_some_and(|facts| facts.session_domain_dispatched)
    {
        return failed("the provider's external-input detection site never dispatched this domain");
    }

    // Refusal 2: after a NEW writer acquires, the old token is a foreign
    // token and must be refused as revoked, still without sending anything.
    let new_lease = map_provider("acquire new lease", provider.lease_op(
        &handle,
        DesktopLeaseOp::Acquire,
        None,
        instance,
    ))?;
    let Some(new_token) = new_lease.lease_token else {
        return failed("new lease acquire returned no token");
    };
    if new_lease.result != "acquired" {
        return failed(format!("new lease acquire returned {:?}", new_lease.result));
    }
    let epoch_after = new_lease.epoch;
    ev.revocation = Some(RevocationFacts {
        epoch_before,
        epoch_after_new_lease: epoch_after,
        epoch_delta: epoch_after.saturating_sub(epoch_before),
        watcher_settle_ms,
    });
    if epoch_after <= epoch_before {
        return failed(format!(
            "the external device event did not bump the domain epoch ({epoch_before} -> {epoch_after})"
        ));
    }

    let pointer_before_second = probe
        .pointer()
        .map_err(|fault| Stop::Failed(format!("XQueryPointer before second refusal: {}", fault.describe())))?;
    let second_attempt = provider.inject(
        &handle,
        DesktopInjectRequest {
            lease_token,
            actions: vec![DesktopAction::PointerMove { x: 9, y: 9 }],
            observation: observation.clone(),
            require_pixel_fresh: false,
            checkpoint: Box::new(|| Ok(())),
        },
        EntityId::new(),
    );
    let pointer_after_second = probe
        .pointer()
        .map_err(|fault| Stop::Failed(format!("XQueryPointer after second refusal: {}", fault.describe())))?;
    let second_refusal = refusal_facts(
        "after_new_writer_acquired",
        lease_token,
        &observation_id,
        second_attempt,
        pointer_before_second,
        pointer_after_second,
    );
    let second_reason = second_refusal.reason.clone();
    let second_outcome = second_refusal.outcome;
    ev.old_lease_refusals.push(second_refusal);
    if second_outcome != "rejected" || second_reason.as_deref() != Some("lease_revoked") {
        return failed(format!(
            "old-lease input after a new writer acquired returned {second_outcome} {second_reason:?} \
             (expected rejected/lease_revoked)"
        ));
    }
    if pointer_before_second != pointer_after_second {
        return failed("pointer moved on a lease_revoked batch");
    }

    // The domain must remain usable for the new epoch: observe again and
    // inject; the provider re-verifies the fresh epoch before every event.
    let fresh_frame = map_provider("observe after handover", provider.observe(&handle, None))?;
    if fresh_frame.lease_epoch != epoch_after {
        return failed(format!(
            "fresh observation captured at epoch {} while the live epoch is {epoch_after}",
            fresh_frame.lease_epoch
        ));
    }
    let (fresh_observation, fresh_observation_id) =
        build_observation(&store, &handle, &fresh_frame)?;
    let delivered = map_provider(
        "inject with the new lease",
        provider.inject(
            &handle,
            DesktopInjectRequest {
                lease_token: new_token,
                actions: vec![DesktopAction::PointerMove { x: 11, y: 13 }],
                observation: fresh_observation,
                require_pixel_fresh: false,
                checkpoint: Box::new(|| Ok(())),
            },
            EntityId::new(),
        ),
    )?;
    let pointer_after_delivery = probe
        .pointer()
        .map_err(|fault| Stop::Failed(format!("XQueryPointer after handover: {}", fault.describe())))?;
    ev.new_lease = Some(NewLeaseFacts {
        result: new_lease.result.into(),
        epoch: epoch_after,
        lease_token: new_token.to_string(),
        fresh_observation_id,
        fresh_epoch_at_capture: fresh_frame.lease_epoch,
        injected_sent_events: delivered.sent_events,
        postcondition_verified: delivered.postcondition_verified,
        pointer_after: pointer_after_delivery,
    });
    if delivered.sent_events != 1 || !delivered.postcondition_verified {
        return failed(format!(
            "the new writer's pointer_move delivered {} events (postcondition_verified={})",
            delivered.sent_events, delivered.postcondition_verified
        ));
    }
    if pointer_after_delivery != [11, 13] {
        return failed(format!(
            "the new writer's pointer_move postcondition was not observed: {pointer_after_delivery:?}"
        ));
    }

    let closed = map_provider("close session", provider.close(&handle, instance))?;
    ev.session_close = Some(CloseFacts {
        held_released: closed.held_released,
        lease_revoked: closed.lease_revoked,
    });
    ev.artifacts.push(run_dir.join("store").display().to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// Observation record (same identity the product computes)
// ---------------------------------------------------------------------------

fn build_observation(
    store: &Store,
    handle: &DesktopSessionHandle,
    frame: &shenko_exec_host::DesktopFrame,
) -> Result<(DesktopObservation, String), Stop> {
    // We only assert the layout this harness proves; an unexpected provider
    // pixel format is a real contradiction, never silently relabelled.
    let media_type = match frame.pixel_format.name.as_str() {
        "x11_zpixmap" => "application/x-x11-zpixmap",
        other => {
            return failed(format!(
                "x11 provider reported unexpected pixel format {other:?}"
            ))
        }
    };
    let raw_ref = store
        .cas
        .seal_object(
            &frame.raw_pixels,
            media_type,
            "computer-use-effects",
            "computer-use-pixels",
            1,
        )
        .map_err(|error| Stop::Failed(format!("seal raw pixels: {error}")))?
        .object;
    let png = encode_png(frame.width, frame.height, &frame.rgb)
        .map_err(|reason| Stop::Failed(format!("encode png preview: {reason}")))?;
    let png_ref = store
        .cas
        .seal_object(
            &png,
            "image/png",
            "computer-use-effects",
            "computer-use-preview",
            1,
        )
        .map_err(|error| Stop::Failed(format!("seal png preview: {error}")))?
        .object;
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
    let sealed = store
        .cas
        .seal_object(
            &serde_json::to_vec(&observation)
                .map_err(|error| Stop::Failed(format!("serialize observation: {error}")))?,
            "application/json",
            "computer-use-effects",
            "computer-use-observation-record",
            1,
        )
        .map_err(|error| Stop::Failed(format!("seal observation record: {error}")))?;
    Ok((observation, sealed.object.digest.0))
}

fn observation_facts(
    observation: &DesktopObservation,
    observation_id: &str,
    raw_pixels: &[u8],
    png_digest: &str,
) -> ObservationFacts {
    ObservationFacts {
        observation_id: observation_id.to_string(),
        frame_id: observation.frame_id,
        revision: observation.revision,
        lease_epoch_at_capture: observation.lease_epoch_at_capture,
        region: observation.region,
        viewport: observation.viewport,
        layout_fingerprint: observation.layout_fingerprint.clone(),
        pixel_hash: observation.pixel_hash.clone(),
        raw_pixels_digest: hex_digest(raw_pixels),
        png_preview_digest: png_digest.to_string(),
        captured_at_ms: observation.captured_at_ms,
    }
}

fn session_facts(
    handle: &DesktopSessionHandle,
    display: &shenko_exec_host::DesktopDisplay,
) -> SessionFacts {
    let detail = &display.detail;
    let text = |key: &str| detail.get(key).and_then(|value| value.as_str()).map(str::to_string);
    let number = |key: &str| detail.get(key).and_then(|value| value.as_u64()).unwrap_or(0);
    let ids = detail
        .get("xtest_source_device_ids")
        .and_then(|value| value.as_array())
        .map(|items| -> Vec<u16> {
            items
                .iter()
                .filter_map(|item| item.as_u64().and_then(|id| u16::try_from(id).ok()))
                .collect()
        })
        .unwrap_or_default();
    SessionFacts {
        session_id: handle.session_id.to_string(),
        provider: display.provider.clone(),
        session_kind: display.session_kind.clone(),
        domain_id: display.domain_id.clone(),
        display: display.display_id.clone(),
        display_id: display.display_id.clone(),
        root_window: number("root_window"),
        screen_num: number("screen_num"),
        width_px: u64::from(display.width_px),
        height_px: u64::from(display.height_px),
        dpi_x: display.dpi_x,
        dpi_y: display.dpi_y,
        depth: u64::from(display.depth),
        xtest_version: text("xtest_version"),
        xinput2_version: text("xinput2_version"),
        xtest_source_device_ids: ids,
        external_takeover_detection: detail
            .get("external_takeover_detection")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

fn refusal_facts(
    phase: &'static str,
    lease_token: EntityId,
    observation_id: &str,
    attempt: Result<shenko_exec_host::DesktopInjectOutcome, DesktopError>,
    pointer_before: [i16; 2],
    pointer_after: [i16; 2],
) -> RefusalFacts {
    match attempt {
        Err(DesktopError::Rejected { reason, detail }) => RefusalFacts {
            phase,
            lease_token: lease_token.to_string(),
            observation_id: observation_id.to_string(),
            actions: vec!["pointer_move".into()],
            outcome: "rejected",
            reason: Some(reason),
            detail: Some(detail),
            sent_events: Some(0),
            pointer_before,
            pointer_after,
        },
        Ok(outcome) => RefusalFacts {
            phase,
            lease_token: lease_token.to_string(),
            observation_id: observation_id.to_string(),
            actions: vec!["pointer_move".into()],
            outcome: "accepted",
            reason: None,
            detail: outcome.stopped_reason.clone(),
            sent_events: Some(outcome.sent_events),
            pointer_before,
            pointer_after,
        },
        Err(other) => RefusalFacts {
            phase,
            lease_token: lease_token.to_string(),
            observation_id: observation_id.to_string(),
            actions: vec!["pointer_move".into()],
            outcome: "error",
            reason: Some(other.reason().to_string()),
            detail: Some(other.detail().to_string()),
            sent_events: None,
            pointer_before,
            pointer_after,
        },
    }
}

fn action_label(action: &DesktopAction) -> String {
    match action {
        DesktopAction::Button { button, press } => {
            format!("button {button} {}", press_label(*press))
        }
        DesktopAction::Key { key, press } => format!(
            "key {key} {}",
            match press {
                Some(press) => press_label(*press),
                None => "tap",
            }
        ),
        other => other.kind().to_string(),
    }
}

fn press_label(press: DesktopPress) -> &'static str {
    match press {
        DesktopPress::Tap => "tap",
        DesktopPress::Down => "down",
        DesktopPress::Up => "up",
    }
}
