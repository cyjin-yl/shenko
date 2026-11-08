//! Bounded real-seat proof: spawn the actual `omp --mode rpc` binary through
//! OmpRpcSeat and verify the adapter discovers a live seat, reports its
//! structured-question capability, and observes without error. This proves
//! the real process is drivable over the JSON-RPC stdio protocol — it does
//! NOT claim a full question→answer journey (that needs an LLM round-trip).
//! Gated on SHENKO_REAL_OMP pointing at the omp binary.

use anyhow::Result;
use shenko_exec_ssh::seat::omp_rpc::OmpRpcSeat;
use shenko_exec_ssh::seat::{SeatAdapter, SeatAdapterId};
use std::path::PathBuf;

fn omp_bin() -> Option<PathBuf> {
    let p = std::env::var_os("SHENKO_REAL_OMP").map(PathBuf::from)?;
    if p.is_file() { Some(p) } else { None }
}

#[tokio::test]
async fn real_omp_rpc_seat_discovers_and_observes() -> Result<()> {
    let Some(omp) = omp_bin() else {
        eprintln!("SKIP: SHENKO_REAL_OMP not set/absent");
        return Ok(());
    };
    let seat = OmpRpcSeat::spawn(
        vec![omp.to_string_lossy().into_owned(), "--mode".into(), "rpc".into()],
        None,
    )
    .await?;

    // Adapter identity + capability are the contract a real seat promises.
    assert_eq!(seat.adapter_id(), SeatAdapterId::OmpRpc);
    let capability = seat.capability();
    assert!(capability.structured_questions);
    assert!(capability.structured_answers);
    assert!(capability.steer && capability.interrupt && capability.resume);

    // discover yields one live handle keyed by the spawn command.
    let handles = seat.discover().await?;
    assert_eq!(handles.len(), 1);
    let handle = &handles[0];
    assert_eq!(handle.adapter, SeatAdapterId::OmpRpc);
    assert!(handle.location_id.contains("omp"), "location_id={}", handle.location_id);
    assert!(handle.current_command.is_some());

    // Poll observe for OMP's `ready` frame. A bare `omp --mode rpc` with no
    // configured model refuses to start the protocol ("No models available")
    // and never emits `ready` — that is honest adapter state, not a defect.
    // The seat's spawn/discover/capability/observe path is what this test
    // proves; a full ready→question journey needs a provisioned model, which
    // is an environment prerequisite outside this crate's control.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut saw_ready_pending = false;
    while tokio::time::Instant::now() < deadline {
        let observation = seat.observe(handle).await?;
        if observation.unknown.iter().any(|u| u.contains("ready")) {
            saw_ready_pending = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    eprintln!("seat protocol state: {}",
        if saw_ready_pending { "ready frame received" }
        else { "spawned; no ready frame (model not provisioned) — adapter reports unknown honestly" });

    seat.close().await;
    Ok(())
}
