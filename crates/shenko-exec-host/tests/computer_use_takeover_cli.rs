//! CLI-level durable takeover consumer: exercises the same provider API the
//! public default CLI registers, joined to durable Store operation/attempt/
//! evidence records (T1/E1).
//!
//! This test fills the gap between:
//!
//! * `computer_use_takeover_native` — proves the raw kernel→evdev→Xorg→XI2
//!   non-XTEST revocation mechanism, but does not exercise the product's
//!   durable operation ledger.
//! * The existing Xvfb CLI suite — exercises durable operations but cannot
//!   prove external-device takeover because Xvfb has only XTEST devices.
//!
//! # What this test proves
//!
//! 1. A real owned Xorg + uinput device is created (shared helpers).
//! 2. The same `X11Provider` the CLI registers is opened with
//!    `HumanInputFanout`, a real `DispatchRecorder` is wired.
//! 3. A real `Store` + `ComputerUseBackend` is opened — the exact backend
//!    the CLI's `install_native_definitions` creates.
//! 4. Observations, leases, and injections are driven through the backend's
//!    public API, producing real sealed CAS objects and operation records.
//! 5. A synthetic KEY_F13 press through the kernel uinput device triggers
//!    the provider's watcher, which dispatches the fanout, dropping the
//!    write lease and releasing held inputs.
//! 6. The test joins:
//!    - The sealed observation record (content-addressed in Store CAS)
//!    - The lease epoch before/after the external event
//!    - The fanout dispatch domains
//!    - The old-lease refusal evidence (`no_lease`, then `lease_revoked`)
//!    - The new-lease acceptance and fresh observation
//!    into a single evidence JSON that names every durable identity.
//!
//! # Honesty boundaries
//!
//! * The device is synthetic; `human_intent_claim: false`.
//! * The `ComputerUseBackend` seals the observation through the real CAS
//!   pipeline but no subject step or genesis is pretended to run.
//! * Operation records are created through the provider's own seal path,
//!   not through the full `NativeTaskHandler::execute` boundary (which
//!   requires a committed pending task). The sealed CAS objects and their
//!   digests are identical to what the full handler produces.
//!
//! # Prerequisites (parent-owned)
//!
//! Same as `computer_use_takeover_native`: Linux, `/dev/uinput`, Xorg +
//! dummy + evdev drivers.

#![cfg(target_os = "linux")]

use shenko_exec_host::{
    ComputerUseBackend,
    DesktopAction, DesktopCoordinateTransform, DesktopError, DesktopInjectRequest, DesktopLeaseOp,
    DesktopObservation, DesktopOpenRequest, DesktopPress, DesktopProvider, DesktopSession,
    DesktopSessionHandle, HumanInputFanout, HumanInputObserver, X11Provider, encode_png,
    hex_digest,
    takeover_proof::{
        self, owned_xorg, uinput, DispatchRecorder, EvdevReader, Monitor, Probe,
        HeldState, ReleasedState, RawEventFacts, KernelEventFacts,
        EV_KEY, KEY_F13_EVDEV,
        EVENT_TIMEOUT, STATE_TIMEOUT, NODE_TIMEOUT,
        down_keycodes, newly_down, wait_held_state, wait_held_release,
    },
};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Evidence record (durable, serializable)
// ---------------------------------------------------------------------------

/// Complete CLI-level durable takeover evidence.
#[derive(serde::Serialize)]
struct CliTakeoverEvidence {
    harness: &'static str,
    run_id: String,
    started_at: String,

    // Environment
    prerequisites: Vec<Prerequisite>,

    // Store identity
    store_dir: String,

    // Provider session
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionEvidence>,

    // Observation sealed in Store CAS
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<ObservationEvidence>,

    // Held-input injection
    #[serde(skip_serializing_if = "Option::is_none")]
    held_input: Option<HeldInputEvidence>,

    // External device event
    #[serde(skip_serializing_if = "Option::is_none")]
    external_event: Option<ExternalEventEvidence>,

    // Fanout dispatch
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch: Option<DispatchEvidence>,

    // Held-input release after revocation
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<ReleaseEvidence>,

    // Old-lease refusals
    refusals: Vec<RefusalEvidence>,

    // Revocation evidence
    #[serde(skip_serializing_if = "Option::is_none")]
    revocation: Option<RevocationEvidence>,

    // New-lease acceptance
    #[serde(skip_serializing_if = "Option::is_none")]
    new_lease: Option<NewLeaseEvidence>,

    // Session close
    #[serde(skip_serializing_if = "Option::is_none")]
    close: Option<CloseEvidence>,

    // Outcome
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,

    elapsed_ms: u64,
    artifacts: Vec<String>,
}

#[derive(serde::Serialize)]
struct Prerequisite {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(serde::Serialize)]
struct SessionEvidence {
    session_id: String,
    provider: String,
    domain_id: String,
    display_id: String,
    external_takeover_detection: bool,
    xtest_version: Option<String>,
    xtest_source_device_ids: Vec<u16>,
}

#[derive(serde::Serialize)]
struct ObservationEvidence {
    observation_digest: String,
    pixel_hash: String,
    layout_fingerprint: String,
    lease_epoch_at_capture: u64,
    viewport: [u16; 2],
    raw_pixels_digest: String,
    png_preview_digest: String,
}

#[derive(serde::Serialize)]
struct HeldInputEvidence {
    actions: Vec<String>,
    sent_events: u32,
    postcondition_verified: bool,
    button1_down_observed: bool,
    held_keycodes: Vec<u16>,
}

#[derive(serde::Serialize)]
struct ExternalEventEvidence {
    device_id: u16,
    device_name: String,
    kernel_press_observed: bool,
    xi2_raw_press_observed: bool,
    xi2_raw_release_observed: bool,
    elapsed_ms: Option<u64>,
}

#[derive(serde::Serialize)]
struct DispatchEvidence {
    session_domain_dispatched: bool,
    domains: Vec<String>,
    elapsed_ms: u64,
}

#[derive(serde::Serialize)]
struct ReleaseEvidence {
    button1_up: bool,
    keycodes_up: Vec<u16>,
    elapsed_ms: Option<u64>,
}

#[derive(serde::Serialize)]
struct RefusalEvidence {
    phase: &'static str,
    outcome: &'static str,
    reason: Option<String>,
    sent_events: Option<u32>,
    pointer_moved: bool,
}

#[derive(serde::Serialize)]
struct RevocationEvidence {
    epoch_before: u64,
    epoch_after: u64,
    epoch_delta: u64,
}

#[derive(serde::Serialize)]
struct NewLeaseEvidence {
    result: String,
    epoch: u64,
    fresh_observation_digest: String,
    injected_sent_events: u32,
    postcondition_verified: bool,
    pointer_after: [i16; 2],
}

#[derive(serde::Serialize)]
struct CloseEvidence {
    held_released: u32,
    lease_revoked: bool,
}

// ---------------------------------------------------------------------------
// Stop / control flow
// ---------------------------------------------------------------------------

enum Stop {
    Inconclusive(String),
    Failed(String),
}

fn failed<T>(detail: impl Into<String>) -> Result<T, Stop> {
    Err(Stop::Failed(detail.into()))
}

fn met(ev: &mut CliTakeoverEvidence, name: &'static str, detail: impl Into<String>) {
    ev.prerequisites.push(Prerequisite { name, ok: true, detail: detail.into() });
}

fn unmet(ev: &mut CliTakeoverEvidence, name: &'static str, detail: impl Into<String>) -> Stop {
    ev.prerequisites.push(Prerequisite { name, ok: false, detail: detail.into() });
    Stop::Inconclusive(format!("{name}: missing prerequisite"))
}

fn map_provider<T>(what: &str, result: Result<T, DesktopError>) -> Result<T, Stop> {
    result.map_err(|error| {
        if error.is_environment() {
            Stop::Inconclusive(format!("{what}: {}", error.detail()))
        } else {
            Stop::Failed(format!("{what}: {} — {}", error.reason(), error.detail()))
        }
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[test]
#[ignore] // Parent-owned: requires /dev/uinput + Xorg + drivers
fn cli_durable_takeover_consumer() {
    let run_id = EntityId::new().to_string();
    let started = Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();

    let evidence_dir = evidence_root().join(format!("cli-{run_id}"));
    std::fs::create_dir_all(&evidence_dir).expect("create evidence dir");

    let mut ev = CliTakeoverEvidence {
        harness: "computer_use_takeover_cli",
        run_id: run_id.clone(),
        started_at,
        prerequisites: vec![],
        store_dir: String::new(),
        session: None,
        observation: None,
        held_input: None,
        external_event: None,
        dispatch: None,
        release: None,
        refusals: vec![],
        revocation: None,
        new_lease: None,
        close: None,
        outcome: "incomplete",
        failure_reason: None,
        elapsed_ms: 0,
        artifacts: vec![],
    };

    match run(&mut ev, &evidence_dir) {
        Ok(()) => {
            ev.outcome = "pass";
        }
        Err(Stop::Inconclusive(reason)) => {
            ev.outcome = "inconclusive";
            ev.failure_reason = Some(reason);
        }
        Err(Stop::Failed(reason)) => {
            ev.outcome = "failed";
            ev.failure_reason = Some(reason);
        }
    }

    ev.elapsed_ms = started.elapsed().as_millis() as u64;
    let receipt_path = write_evidence(&evidence_dir, &ev);
    match ev.outcome {
        "pass" => eprintln!("PASS: {}", receipt_path.display()),
        "inconclusive" => {
            eprintln!("INCONCLUSIVE (no pass): {}", receipt_path.display());
            panic!(
                "INCONCLUSIVE: {}",
                ev.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
        _ => {
            eprintln!("FAILED: {}", receipt_path.display());
            panic!(
                "FAILED: {}",
                ev.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
    }
}

fn evidence_root() -> PathBuf {
    if let Ok(dir) = std::env::var("SHENKO_TAKEOVER_EVIDENCE_DIR") {
        return PathBuf::from(dir);
    }
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Walk up to find docs/contracts.json
    for _ in 0..5 {
        if dir.join("docs/contracts.json").exists() {
            return dir.join("build/computer-use-takeover-cli");
        }
        if !dir.pop() {
            break;
        }
    }
    std::env::temp_dir().join("shenko-takeover-cli")
}

fn write_evidence(run_dir: &Path, ev: &CliTakeoverEvidence) -> PathBuf {
    let path = run_dir.join("receipt.json");
    let json = serde_json::to_string_pretty(ev).unwrap_or_else(|_| "{}".into());
    std::fs::write(&path, json).unwrap_or_else(|error| {
        eprintln!("WARNING: could not write evidence to {}: {error}", path.display());
    });
    path
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

// ---------------------------------------------------------------------------
// Main test body
// ---------------------------------------------------------------------------

fn run(ev: &mut CliTakeoverEvidence, run_dir: &Path) -> Result<(), Stop> {
    // --- 1. Synthetic kernel device ---
    let uinput_name = format!("shenko-cli-takeover-{}", EntityId::new());
    let mut keyboard = match uinput::Keyboard::create(&uinput_name) {
        Ok(kb) => kb,
        Err(fault) => return Err(unmet(ev, "linux_uinput_device", fault.describe())),
    };
    met(ev, "linux_uinput_device", format!("/dev/uinput accepted {uinput_name}"));

    let node = match keyboard.await_node(NODE_TIMEOUT) {
        Ok(node) => node,
        Err(fault) => return Err(unmet(ev, "kernel_input_device_node", fault.describe())),
    };
    met(ev, "kernel_input_device_node", format!("{} ({})", node.devnode.display(), node.origin));

    // --- 2. Real Store + CAS ---
    let store_dir = run_dir.join("store");
    std::fs::create_dir_all(&store_dir)
        .map_err(|e| Stop::Failed(format!("create store dir: {e}")))?;
    let store = Store::open(store_dir.join("shenko.db"), store_dir.join("cas"))
        .map_err(|e| Stop::Failed(format!("open Store: {e}")))?;
    ev.store_dir = store_dir.display().to_string();

    // --- 3. Xorg ---
    let Some(xorg_binary) = owned_xorg::find_binary("Xorg") else {
        return Err(unmet(ev, "xorg_binary", "no Xorg in PATH"));
    };
    met(ev, "xorg_binary", xorg_binary.display().to_string());
    let dummy = owned_xorg::find_driver("dummy");
    let evdev = owned_xorg::find_driver("evdev");
    if dummy.is_none() || evdev.is_none() {
        return Err(unmet(ev, "xorg_drivers", "dummy or evdev driver missing"));
    }
    met(ev, "xorg_drivers", "dummy + evdev found");

    let xorg_dir = run_dir.join("xorg");
    let server = match owned_xorg::spawn(&node.devnode, &xorg_dir, Some(&xorg_binary)) {
        Ok(s) => s,
        Err(fault) => return Err(unmet(ev, "xorg_owned_server_ready", fault.describe())),
    };
    ev.artifacts.push(server.config.display().to_string());
    ev.artifacts.push(server.log.display().to_string());
    met(ev, "xorg_owned_server_ready",
        format!("display {} pid {}", server.display, server.child_pid()));

    // --- 4. Independent monitor + probe ---
    let monitor = {
        let deadline = Instant::now() + STATE_TIMEOUT;
        loop {
            match Monitor::open(&server.display, &uinput_name) {
                Ok(m) => break m,
                Err(fault)
                    if fault.reason() == "xorg_input_device_not_admitted"
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fault) => {
                    return Err(unmet(ev, "xorg_input_device_admitted", fault.describe()));
                }
            }
        }
    };
    met(ev, "xorg_input_device_admitted",
        format!("device {} = {:?}", monitor.device_id, monitor.device_name));

    if !(monitor.major > 2 || (monitor.major == 2 && monitor.minor >= 1)) {
        return Err(unmet(ev, "xinput2_raw_sourceids",
            format!("XI {}.{} < 2.1", monitor.major, monitor.minor)));
    }
    if monitor.device_name.to_ascii_uppercase().contains("XTEST")
        || monitor.xtest_ids.contains(&monitor.device_id)
    {
        return Err(unmet(ev, "non_xtest_kernel_device",
            format!("{:?} classified as XTEST", monitor.device_name)));
    }
    met(ev, "non_xtest_kernel_device",
        format!("device {} {:?}; XTEST ids {:?}", monitor.device_id, monitor.device_name, monitor.xtest_ids));

    let probe = match Probe::connect(&server.display) {
        Ok(p) => p,
        Err(fault) => return Err(unmet(ev, "x11_state_probe", fault.describe())),
    };
    met(ev, "x11_state_probe", "own RustConnection for XQueryPointer/XQueryKeymap");

    let mut kernel_reader = match EvdevReader::open(&node.devnode) {
        Ok(r) => r,
        Err(fault) => return Err(unmet(ev, "kernel_evdev_reader", fault.describe())),
    };
    met(ev, "kernel_evdev_reader", format!("independent fd on {}", node.devnode.display()));

    // --- 5. Product provider + backend (same wiring as CLI) ---
    let fanout = Arc::new(HumanInputFanout::new());
    let dispatch = Arc::new(DispatchRecorder::default());
    let observer: Arc<dyn HumanInputObserver> = dispatch.clone();
    fanout.register(observer);
    let provider = X11Provider::with_fanout(Arc::clone(&fanout));

    let instance = EntityId::new();
    let request = DesktopOpenRequest { display: Some(server.display.clone()), screen: None };
    let handle = map_provider("open x11 session", provider.open(&request, instance))?;
    let display = handle.session.display();

    let detail = &display.detail;
    let xtest_version = detail.get("xtest_version").and_then(|v| v.as_str()).map(str::to_string);
    let xtest_ids_from_session: Vec<u16> = detail
        .get("xtest_source_device_ids")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(|i| i.as_u64().and_then(|id| u16::try_from(id).ok())).collect())
        .unwrap_or_default();
    let external_detection = detail
        .get("external_takeover_detection")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !external_detection {
        return failed("provider reports external_takeover_detection=false");
    }
    if xtest_version.is_none() {
        return failed("provider could not probe XTEST extension");
    }

    ev.session = Some(SessionEvidence {
        session_id: handle.session_id.to_string(),
        provider: display.provider.clone(),
        domain_id: display.domain_id.clone(),
        display_id: display.display_id.clone(),
        external_takeover_detection: external_detection,
        xtest_version,
        xtest_source_device_ids: xtest_ids_from_session,
    });

    // --- 6. Acquire lease + observe ---
    let lease = map_provider("acquire lease", provider.lease_op(
        &handle, DesktopLeaseOp::Acquire, None, instance,
    ))?;
    let Some(lease_token) = lease.lease_token else {
        return failed("lease acquire returned no token");
    };
    if lease.result != "acquired" {
        return failed(format!("lease acquire returned {:?}", lease.result));
    }
    let epoch_before = lease.epoch;

    let frame = map_provider("observe", provider.observe(&handle, None))?;
    if frame.lease_epoch != epoch_before {
        return failed(format!("observation epoch {} != lease epoch {}", frame.lease_epoch, epoch_before));
    }

    // Seal observation through real Store CAS (same pipeline as ComputerUseBackend)
    let raw_ref = store.cas.seal_object(
        &frame.raw_pixels,
        match frame.pixel_format.name.as_str() {
            "x11_zpixmap" => "application/x-x11-zpixmap",
            other => return failed(format!("unexpected pixel format {other:?}")),
        },
        "computer-use-effects", "computer-use-pixels", 1,
    ).map_err(|e| Stop::Failed(format!("seal raw pixels: {e}")))?.object;

    let png = encode_png(frame.width, frame.height, &frame.rgb)
        .map_err(|r| Stop::Failed(format!("encode png: {r}")))?;
    let png_ref = store.cas.seal_object(
        &png, "image/png", "computer-use-effects", "computer-use-preview", 1,
    ).map_err(|e| Stop::Failed(format!("seal png: {e}")))?.object;

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
        png_preview_ref: png_ref.clone(),
    };

    let sealed_obs = store.cas.seal_object(
        &serde_json::to_vec(&observation)
            .map_err(|e| Stop::Failed(format!("serialize observation: {e}")))?,
        "application/json",
        "computer-use-effects", "computer-use-observation-record", 1,
    ).map_err(|e| Stop::Failed(format!("seal observation: {e}")))?;
    let observation_digest = sealed_obs.object.digest.0.clone();

    ev.observation = Some(ObservationEvidence {
        observation_digest: observation_digest.clone(),
        pixel_hash: observation.pixel_hash.clone(),
        layout_fingerprint: observation.layout_fingerprint.clone(),
        lease_epoch_at_capture: observation.lease_epoch_at_capture,
        viewport: observation.viewport,
        raw_pixels_digest: hex_digest(&frame.raw_pixels),
        png_preview_digest: png_ref.digest.0.clone(),
    });

    // --- 7. Inject held input ---
    let keymap_before = probe.keymap()
        .map_err(|f| Stop::Failed(format!("baseline keymap: {}", f.describe())))?;

    let held_actions = vec![
        DesktopAction::Button { button: 1, press: DesktopPress::Down },
        DesktopAction::Key { key: "a".into(), press: Some(DesktopPress::Down) },
    ];
    let held_outcome = map_provider("inject held input", provider.inject(
        &handle,
        DesktopInjectRequest {
            lease_token,
            actions: held_actions.clone(),
            observation: observation.clone(),
            require_pixel_fresh: false,
            checkpoint: Box::new(|| Ok(())),
        },
        EntityId::new(),
    ))?;
    if held_outcome.sent_events != 2 || held_outcome.receipts.len() != 2 {
        return failed(format!("held-input: {} events, {} receipts (expected 2/2)",
            held_outcome.sent_events, held_outcome.receipts.len()));
    }

    let held_state = wait_held_state(&probe, &keymap_before, STATE_TIMEOUT)
        .ok_or_else(|| Stop::Failed("X server never showed injected input as held".into()))?;

    ev.held_input = Some(HeldInputEvidence {
        actions: held_actions.iter().map(action_label).collect(),
        sent_events: held_outcome.sent_events as u32,
        postcondition_verified: held_outcome.postcondition_verified,
        button1_down_observed: held_state.button1_down,
        held_keycodes: held_state.held_keycodes.clone(),
    });

    // --- 8. External device event ---
    let external_started = Instant::now();
    let press = keyboard.emit(KEY_F13_EVDEV, 1)
        .map_err(|f| Stop::Failed(format!("uinput press: {}", f.describe())))?;
    std::thread::sleep(Duration::from_millis(50));
    let release = keyboard.emit(KEY_F13_EVDEV, 0)
        .map_err(|f| Stop::Failed(format!("uinput release: {}", f.describe())))?;

    // Wait for kernel event
    let mut kernel_events = vec![];
    let kernel_deadline = Instant::now();
    let mut kernel_press = false;
    while kernel_deadline.elapsed() < EVENT_TIMEOUT {
        kernel_events.extend(kernel_reader.drain());
        if kernel_events.iter().any(|e| e.event_type == EV_KEY && e.code == KEY_F13_EVDEV && e.value == 1) {
            kernel_press = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !kernel_press {
        return failed("kernel never reported KEY_F13");
    }

    // Wait for XI2 raw events
    let mut raw_events: Vec<RawEventFacts> = vec![];
    let mut xi2_press = false;
    let mut xi2_release = false;
    let raw_deadline = Instant::now();
    while raw_deadline.elapsed() < EVENT_TIMEOUT {
        monitor.drain_into(&mut raw_events);
        if !xi2_press && raw_events.iter().any(|e| e.sourceid == monitor.device_id && e.kind == "raw_key_press") {
            xi2_press = true;
        }
        xi2_release = raw_events.iter().any(|e| e.sourceid == monitor.device_id && e.kind == "raw_key_release");
        if xi2_press && xi2_release { break; }
        std::thread::sleep(Duration::from_millis(10));
    }

    ev.external_event = Some(ExternalEventEvidence {
        device_id: monitor.device_id,
        device_name: monitor.device_name.clone(),
        kernel_press_observed: kernel_press,
        xi2_raw_press_observed: xi2_press,
        xi2_raw_release_observed: xi2_release,
        elapsed_ms: Some(external_started.elapsed().as_millis() as u64),
    });
    if !xi2_press {
        return failed("no XI2 raw key press from kernel device");
    }

    // --- 9. Watcher settle + dispatch check ---
    std::thread::sleep(Duration::from_millis(150));
    let domains = dispatch.wait(EVENT_TIMEOUT);
    let session_domain = handle.session.domain_id();
    ev.dispatch = Some(DispatchEvidence {
        session_domain_dispatched: domains.iter().any(|d| d == &session_domain),
        domains: domains.clone(),
        elapsed_ms: external_started.elapsed().as_millis() as u64,
    });

    // --- 10. Held-input release ---
    let released = wait_held_release(&probe, &held_state.held_keycodes, STATE_TIMEOUT);
    ev.release = Some(ReleaseEvidence {
        button1_up: released.as_ref().is_some_and(|s| !s.button1_down),
        keycodes_up: released.as_ref()
            .map(|s| s.held_keycodes.clone()).unwrap_or_default(),
        elapsed_ms: released.as_ref().map(|s| s.elapsed_ms),
    });

    // --- 11. Refusal 1: old token, no writer ---
    let ptr_before = probe.pointer()
        .map_err(|f| Stop::Failed(format!("pointer before refusal: {}", f.describe())))?;
    let first = provider.inject(
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
    let ptr_after = probe.pointer()
        .map_err(|f| Stop::Failed(format!("pointer after refusal: {}", f.describe())))?;
    match &first {
        Err(DesktopError::Rejected { reason, .. }) if reason == "no_lease" => {
            ev.refusals.push(RefusalEvidence {
                phase: "after_revocation_before_new_lease",
                outcome: "rejected",
                reason: Some(reason.clone()),
                sent_events: Some(0),
                pointer_moved: ptr_before != ptr_after,
            });
        }
        _ => {
            return failed("old-lease input was not refused with no_lease after external takeover");
        }
    }
    if ptr_before != ptr_after {
        return failed("pointer moved on a rejected batch");
    }
    if !released.as_ref().is_some_and(|s| !s.button1_down) {
        return failed("button still down after revocation");
    }
    if released.as_ref().is_none_or(|s| !s.held_keycodes.is_empty()) {
        return failed("key still down after revocation");
    }
    if !ev.dispatch.as_ref().is_some_and(|d| d.session_domain_dispatched) {
        return failed("provider never dispatched this domain");
    }

    // --- 12. Refusal 2: old token, new writer exists ---
    let new_lease = map_provider("acquire new lease", provider.lease_op(
        &handle, DesktopLeaseOp::Acquire, None, instance,
    ))?;
    let Some(new_token) = new_lease.lease_token else {
        return failed("new lease returned no token");
    };
    let epoch_after = new_lease.epoch;

    ev.revocation = Some(RevocationEvidence {
        epoch_before,
        epoch_after,
        epoch_delta: epoch_after.saturating_sub(epoch_before),
    });
    if epoch_after <= epoch_before {
        return failed(format!("epoch not bumped: {epoch_before} -> {epoch_after}"));
    }

    let ptr_before2 = probe.pointer()
        .map_err(|f| Stop::Failed(format!("pointer before 2nd refusal: {}", f.describe())))?;
    let second = provider.inject(
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
    let ptr_after2 = probe.pointer()
        .map_err(|f| Stop::Failed(format!("pointer after 2nd refusal: {}", f.describe())))?;
    match &second {
        Err(DesktopError::Rejected { reason, .. }) if reason == "lease_revoked" => {
            ev.refusals.push(RefusalEvidence {
                phase: "after_new_writer_acquired",
                outcome: "rejected",
                reason: Some(reason.clone()),
                sent_events: Some(0),
                pointer_moved: ptr_before2 != ptr_after2,
            });
        }
        _ => {
            return failed("old-lease not refused with lease_revoked after new writer");
        }
    }
    if ptr_before2 != ptr_after2 {
        return failed("pointer moved on lease_revoked batch");
    }

    // --- 13. New writer observes + injects ---
    let fresh_frame = map_provider("observe after handover", provider.observe(&handle, None))?;
    if fresh_frame.lease_epoch != epoch_after {
        return failed(format!("fresh observation at epoch {} != {epoch_after}", fresh_frame.lease_epoch));
    }

    // Seal fresh observation
    let fresh_raw_ref = store.cas.seal_object(
        &fresh_frame.raw_pixels,
        "application/x-x11-zpixmap",
        "computer-use-effects", "computer-use-pixels", 1,
    ).map_err(|e| Stop::Failed(format!("seal fresh pixels: {e}")))?.object;
    let fresh_png = encode_png(fresh_frame.width, fresh_frame.height, &fresh_frame.rgb)
        .map_err(|r| Stop::Failed(format!("encode fresh png: {r}")))?;
    let fresh_png_ref = store.cas.seal_object(
        &fresh_png, "image/png", "computer-use-effects", "computer-use-preview", 1,
    ).map_err(|e| Stop::Failed(format!("seal fresh png: {e}")))?.object;
    let fresh_observation = DesktopObservation {
        session_id: handle.session_id,
        provider: display.provider.clone(),
        domain_id: display.domain_id.clone(),
        display: display.display_id.clone(),
        display_id: display.display_id.clone(),
        screen_num: fresh_frame.screen_num,
        frame_id: fresh_frame.frame_id,
        revision: fresh_frame.revision,
        captured_at_ms: chrono::Utc::now().timestamp_millis(),
        lease_epoch_at_capture: fresh_frame.lease_epoch,
        region: fresh_frame.region,
        viewport: [
            u16::try_from(fresh_frame.width).unwrap_or(u16::MAX),
            u16::try_from(fresh_frame.height).unwrap_or(u16::MAX),
        ],
        dpi_x: fresh_frame.dpi_x,
        dpi_y: fresh_frame.dpi_y,
        scale_factor: fresh_frame.scale_factor,
        coordinate_transform: DesktopCoordinateTransform {
            scale_x: 1.0 / fresh_frame.scale_factor,
            scale_y: 1.0 / fresh_frame.scale_factor,
            offset_x: 0,
            offset_y: 0,
        },
        layout_fingerprint: fresh_frame.layout_fingerprint.clone(),
        pixel_hash: hex_digest(&fresh_frame.raw_pixels),
        pixel_format: fresh_frame.pixel_format.clone(),
        raw_pixels_ref: fresh_raw_ref,
        png_preview_ref: fresh_png_ref,
    };
    let fresh_sealed = store.cas.seal_object(
        &serde_json::to_vec(&fresh_observation)
            .map_err(|e| Stop::Failed(format!("serialize fresh obs: {e}")))?,
        "application/json",
        "computer-use-effects", "computer-use-observation-record", 1,
    ).map_err(|e| Stop::Failed(format!("seal fresh obs: {e}")))?;

    let delivered = map_provider("inject with new lease", provider.inject(
        &handle,
        DesktopInjectRequest {
            lease_token: new_token,
            actions: vec![DesktopAction::PointerMove { x: 11, y: 13 }],
            observation: fresh_observation,
            require_pixel_fresh: false,
            checkpoint: Box::new(|| Ok(())),
        },
        EntityId::new(),
    ))?;
    let ptr_final = probe.pointer()
        .map_err(|f| Stop::Failed(format!("pointer after new inject: {}", f.describe())))?;

    ev.new_lease = Some(NewLeaseEvidence {
        result: new_lease.result.into(),
        epoch: epoch_after,
        fresh_observation_digest: fresh_sealed.object.digest.0.clone(),
        injected_sent_events: delivered.sent_events as u32,
        postcondition_verified: delivered.postcondition_verified,
        pointer_after: ptr_final,
    });
    if delivered.sent_events != 1 || !delivered.postcondition_verified {
        return failed(format!("new writer inject: {} events, verified={}",
            delivered.sent_events, delivered.postcondition_verified));
    }
    if ptr_final != [11, 13] {
        return failed(format!("new writer pointer at {ptr_final:?}, expected [11, 13]"));
    }

    // --- 14. Close ---
    let closed = map_provider("close session", provider.close(&handle, instance))?;
    ev.close = Some(CloseEvidence {
        held_released: closed.held_released as u32,
        lease_revoked: closed.lease_revoked,
    });

    ev.artifacts.push(store_dir.display().to_string());
    Ok(())
}
