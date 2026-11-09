//! C06/R018-in-WASM: run the real `ssh_banner.wasm` guest against a live SSH
//! server and prove the SSH protocol exchange happens INSIDE the module over
//! a host-preopened socket — not by forwarding a host ssh's stdout.
//!
//! The guest reads `SHENKO_NET_0`, reads the server `SSH-2.0-...` banner,
//! sends its own client banner, sends a framed SSH_MSG_KEXINIT, and reads the
//! server's KEXINIT reply. The test asserts the captured banner starts
//! `SSH-` and the server replied with a type-20 (KEXINIT) packet.
//!
//! Gated on SHENKO_REAL_SSH_NET = "host:port" of a live SSH server
//! (e.g. 10.10.10.12:22) and SHENKO_SSH_WASM = path to ssh_banner.wasm.

use anyhow::Result;
use shenko_wasm::{
    MemoryOutputPipe, MountMode, MountSpec, NetworkAdmission, WasiCommand, WasiRunControl,
    WasiRunOutcome, WasmRuntime, HOME_GUEST_PATH,
};
use std::path::PathBuf;
use std::time::Duration;

fn gated() -> Option<(String, u16, PathBuf)> {
    let target = std::env::var("SHENKO_REAL_SSH_NET").ok()?;
    let wasm = std::env::var("SHENKO_SSH_WASM").ok().map(PathBuf::from)?;
    if !wasm.is_file() {
        return None;
    }
    let (host, port) = target.rsplit_once(':')?;
    Some((host.into(), port.parse().ok()?, wasm))
}

#[test]
fn ssh_banner_exchange_runs_inside_the_module() -> Result<()> {
    let Some((host, port, wasm)) = gated() else {
        eprintln!("SKIP: SHENKO_REAL_SSH_NET / SHENKO_SSH_WASM not set");
        return Ok(());
    };
    let module = std::fs::read(&wasm)?;

    let home = std::env::temp_dir().join(format!(
        "shenko-ssh-wasm-{}",
        shenko_types::ids::EntityId::new()
    ));
    std::fs::create_dir_all(&home)?;
    let mut runtime = WasmRuntime::with_mounts(vec![MountSpec::new(
        home,
        HOME_GUEST_PATH,
        MountMode::ReadOnly,
        false,
        false,
    )])?;

    let stdout = MemoryOutputPipe::new(64 * 1024);
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let mut command = WasiCommand::new(std::io::empty(), stdout.clone(), stderr.clone());
    command.network = Some(NetworkAdmission::for_installed_target(&host, port)?);
    command.control = WasiRunControl {
        deadline: Some(Duration::from_secs(20)),
        cancel: None,
        revocation: None,
    };

    let outcome = runtime.run_wasi_module(&module, command)?;
    let out = String::from_utf8_lossy(&stdout.contents()).to_string();
    let err = String::from_utf8_lossy(&stderr.contents()).to_string();
    eprintln!("guest stderr: {err}\nguest stdout: {out}");

    // The guest exits 0 only after seeing the server's KEXINIT (msg type 20).
    let code = match outcome {
        WasiRunOutcome::Exited(c) => c,
        other => panic!("expected Exited, got {other:?}; stderr={err}"),
    };
    assert_eq!(code, 0, "guest SSH exchange failed; stderr={err} stdout={out}");

    // In-module proof: the guest's own stdout records the exchange it ran.
    assert!(err.contains("SERVER_BANNER=SSH-"), "no server banner observed in-module: {err}");
    assert!(err.contains("MSG_TYPE=20"), "no KEXINIT reply observed in-module: {err}");
    assert!(out.contains("SSH_EXCHANGE_OK"), "exchange did not complete: {out}");
    Ok(())
}
