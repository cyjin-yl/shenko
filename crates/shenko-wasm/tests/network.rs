//! Integration coverage for the governed WASI network transport.
//!
//! Every scenario is a real preview1 guest module and a real loopback TCP
//! peer: the host opens a socket only for a target an admission names, the
//! guest performs the I/O through the preview1 socket surface
//! (`fd_read`/`fd_write`/`sock_shutdown`), and the run's stop conditions end a
//! guest that is blocked on a peer which never answers.
//!
//! Descriptor numbering is part of the contract: the module mounts `/home` as
//! its only preopen, so the admitted socket is the next descriptor (`fd 4`),
//! published to the guest shim in `SHENKO_NET_0`.
use anyhow::Result;
use shenko_wasm::{
    AdmittedTarget, MemoryOutputPipe, MountMode, MountSpec, NetworkAdmission, NetworkOperation,
    WasiCancellation, WasiCommand, WasiRunControl, WasiRunOutcome, WasmRuntime, HOME_GUEST_PATH,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// `/home` is the single directory preopen, so an admitted socket takes fd 4.
const SOCKET_FD: i32 = 4;

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shenko-wasm-net-{tag}-{}",
        shenko_types::ids::EntityId::new()
    ));
    std::fs::create_dir_all(&dir).expect("create temp home");
    dir
}

fn runtime(tag: &str) -> WasmRuntime {
    let home = temp_home(tag);
    WasmRuntime::with_mounts(vec![MountSpec::new(
        home,
        HOME_GUEST_PATH,
        MountMode::ReadOnly,
        false,
        false,
    )])
    .expect("runtime with /home mount")
}

fn stdout_command() -> (
    WasiCommand<std::io::Empty, MemoryOutputPipe, std::io::Empty>,
    MemoryOutputPipe,
) {
    let stdout = MemoryOutputPipe::new(64 * 1024);
    (
        WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty()),
        stdout,
    )
}

fn exit_code(outcome: WasiRunOutcome) -> i32 {
    match outcome {
        WasiRunOutcome::Exited(code) => code,
        other => panic!("expected Exited, got {other:?}"),
    }
}

fn loopback() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

/// Guest reads once from the admitted socket into memory at 64 and echoes the
/// received bytes to stdout.
const ECHO_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  ;; iovec at 0: { buf = 64, len = 64 }; nread at 8.
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 64))
    (if (i32.ne (call $fd_read (i32.const 4) (i32.const 0) (i32.const 1) (i32.const 8))
                (i32.const 0))
      (then (call $exit (i32.const 71))))
    ;; stdout ciov at 16: { buf = 64, len = nread }; nwritten at 24.
    (i32.store (i32.const 16) (i32.const 64))
    (i32.store (i32.const 20) (i32.load (i32.const 8)))
    (if (i32.ne (call $fd_write (i32.const 1) (i32.const 16) (i32.const 1) (i32.const 24))
                (i32.const 0))
      (then (call $exit (i32.const 72))))
  )
)
"#;

/// Guest sends one payload, shuts its write direction down and exits.
const HALF_CLOSE_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "sock_shutdown"
    (func $sock_shutdown (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 64) "half-close")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 10))
    (if (i32.ne (call $fd_write (i32.const 4) (i32.const 0) (i32.const 1) (i32.const 8))
                (i32.const 0))
      (then (call $exit (i32.const 73))))
    ;; sdflags WR = 2
    (if (i32.ne (call $sock_shutdown (i32.const 4) (i32.const 2)) (i32.const 0))
      (then (call $exit (i32.const 74))))
  )
)
"#;

/// Guest blocks in `fd_read` on a peer that never sends.
const BLOCKED_READ_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 64))
    (drop (call $fd_read (i32.const 4) (i32.const 0) (i32.const 1) (i32.const 8)))
  )
)
"#;

#[test]
fn admission_refuses_contradictory_or_widened_targets() {
    let both = AdmittedTarget::new(
        "127.0.0.1",
        8080,
        false,
        vec![NetworkOperation::Connect, NetworkOperation::Listen],
    );
    assert!(both.is_err(), "connect and listen are mutually exclusive");

    let accept_without_listen = AdmittedTarget::new(
        "127.0.0.1",
        8080,
        false,
        vec![NetworkOperation::Accept],
    );
    assert!(accept_without_listen.is_err(), "accept needs a listener");

    let name_without_resolve = AdmittedTarget::new(
        "localhost",
        8080,
        false,
        vec![NetworkOperation::Connect],
    );
    assert!(name_without_resolve.is_err(), "a name needs resolve");

    assert!(
        NetworkAdmission::new(Vec::new()).is_err(),
        "an empty admission is never an implicit any-target grant"
    );

    let separator_in_host = AdmittedTarget::new(
        "127.0.0.1,evil",
        8080,
        false,
        vec![NetworkOperation::Connect],
    );
    assert!(separator_in_host.is_err(), "descriptor separators are refused");

    let zero_port = AdmittedTarget::new("127.0.0.1", 0, false, vec![NetworkOperation::Connect]);
    assert!(zero_port.is_err(), "port 0 is not a reachable target");
}

#[test]
fn installed_target_admits_exactly_one_outbound_stream() -> Result<()> {
    let admission = NetworkAdmission::for_installed_target("127.0.0.1", 8848)?;
    let targets = admission.targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].host, "127.0.0.1");
    assert_eq!(targets[0].port, 8848);
    assert!(targets[0].permits(NetworkOperation::Connect));
    assert!(!targets[0].permits(NetworkOperation::Listen));
    assert!(
        !targets[0].resolve,
        "an IP literal is used verbatim, never re-resolved"
    );

    // A recorded host name is resolved by the host for that one target.
    let named = NetworkAdmission::for_installed_target("localhost", 8848)?;
    assert!(named.targets()[0].resolve);
    Ok(())
}

#[test]
fn guest_echoes_bytes_from_the_admitted_socket() -> Result<()> {
    let (listener, port) = loopback();
    let payload = b"hello-from-peer";
    let peer = std::thread::spawn(move || -> Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.write_all(payload)?;
        stream.flush()?;
        Ok(())
    });

    let module = wat::parse_str(ECHO_WAT)?;
    let mut runtime = runtime("echo");
    let (mut command, stdout) = stdout_command();
    command.network = Some(NetworkAdmission::for_installed_target("127.0.0.1", port)?);
    command.control = WasiRunControl {
        deadline: Some(Duration::from_secs(10)),
        cancel: None,
    };

    let outcome = runtime.run_wasi_module(&module, command)?;
    peer.join().expect("peer thread")?;
    assert_eq!(exit_code(outcome), 0, "guest must not fail its socket calls");
    assert_eq!(stdout.contents().as_ref(), payload);
    Ok(())
}

#[test]
fn guest_half_close_is_visible_to_the_peer() -> Result<()> {
    let (listener, port) = loopback();
    let peer = std::thread::spawn(move || -> Result<Vec<u8>> {
        let (mut stream, _) = listener.accept()?;
        let mut received = Vec::new();
        stream.read_to_end(&mut received)?;
        Ok(received)
    });

    let module = wat::parse_str(HALF_CLOSE_WAT)?;
    let mut runtime = runtime("half-close");
    let mut command = WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty());
    command.network = Some(NetworkAdmission::for_installed_target("127.0.0.1", port)?);
    command.control = WasiRunControl {
        deadline: Some(Duration::from_secs(10)),
        cancel: None,
    };

    let outcome = runtime.run_wasi_module(&module, command)?;
    let received = peer.join().expect("peer thread")?;
    assert_eq!(exit_code(outcome), 0);
    // read_to_end only returns after the guest's shutdown reaches the peer as
    // an EOF, so the asserted bytes are the real half-close effect.
    assert_eq!(received, b"half-close");
    Ok(())
}

#[test]
fn networked_run_without_a_bound_is_refused_before_any_socket() -> Result<()> {
    let (listener, port) = loopback();
    let module = wat::parse_str(BLOCKED_READ_WAT)?;
    let mut runtime = runtime("unbounded");
    let mut command = WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty());
    command.network = Some(NetworkAdmission::for_installed_target("127.0.0.1", port)?);

    let error = runtime
        .run_wasi_module(&module, command)
        .expect_err("an unbounded networked run must be refused");
    assert!(
        error.to_string().contains("WASI_NETWORK_RUN_UNBOUNDED"),
        "unexpected error: {error:#}"
    );
    // No connection was made: the admission never reached the transport.
    listener.set_nonblocking(true).expect("nonblocking listener");
    assert!(
        listener.accept().is_err(),
        "refusal must happen before the host opens a socket"
    );
    Ok(())
}

#[test]
fn caller_supplied_transport_descriptors_are_refused() -> Result<()> {
    let module = wat::parse_str(BLOCKED_READ_WAT)?;
    let mut runtime = runtime("reserved-env");
    let mut command = WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty());
    command
        .environment
        .push(("SHENKO_NET_0".into(), "4,127.0.0.1,1,connect".into()));
    let error = runtime
        .run_wasi_module(&module, command)
        .expect_err("the host owns the descriptor namespace");
    assert!(
        error.to_string().contains("WASI_COMMAND_INVALID"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[test]
fn cancellation_ends_a_guest_blocked_in_a_socket_read() -> Result<()> {
    let (listener, port) = loopback();
    let peer = std::thread::spawn(move || {
        // Accept and stay silent: the guest never gets its data.
        let (_stream, _) = listener.accept().expect("peer accept");
        std::thread::sleep(Duration::from_millis(1500));
    });

    let module = wat::parse_str(BLOCKED_READ_WAT)?;
    let mut runtime = runtime("cancel");
    let cancel = WasiCancellation::default();
    let handle = cancel.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        handle.cancel();
    });

    let mut command = WasiCommand::new(std::io::empty(), std::io::empty(), std::io::empty());
    command.network = Some(NetworkAdmission::for_installed_target("127.0.0.1", port)?);
    command.control = WasiRunControl {
        deadline: Some(Duration::from_secs(30)),
        cancel: Some(cancel),
    };

    let started = Instant::now();
    let outcome = runtime.run_wasi_module(&module, command)?;
    let elapsed = started.elapsed();
    trigger.join().expect("cancel thread");
    peer.join().expect("peer thread");
    assert_eq!(outcome, WasiRunOutcome::Cancelled);
    assert!(
        elapsed < Duration::from_secs(5),
        "a blocked socket read must end on cancellation, took {elapsed:?}"
    );
    Ok(())
}
