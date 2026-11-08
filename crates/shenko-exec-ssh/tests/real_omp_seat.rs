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

    // With a provisioned OMP model config the real process emits `ready` and
    // begins streaming JSONL frames. Wait for the adapter to see it, then the
    // seat is genuinely protocol-ready (not just spawned).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut ready = false;
    while tokio::time::Instant::now() < deadline {
        let observation = seat.observe(handle).await?;
        if !observation.unknown.iter().any(|u| u.contains("ready")) {
            ready = true;
            eprintln!("OMP ready frame received; questions={} unknown={:?}",
                observation.questions.len(), observation.unknown);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(ready, "omp --mode rpc never emitted its ready frame");

    seat.close().await;
    Ok(())
}
