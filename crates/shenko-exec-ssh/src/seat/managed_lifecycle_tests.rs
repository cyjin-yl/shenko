//! Real child/pipe boundaries with a deliberately small protocol fixture.
//! These checks do not run OMP, Claude, a model, or a coding task.
use super::*;
use crate::seat::{ManagedWriteError, ManagedWriteState};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

fn python(script: &str) -> Vec<String> {
    vec!["python3".into(), "-I".into(), "-u".into(), "-c".into(), script.into()]
}

async fn process_exited(seat: &OmpRpcSeat) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        while seat.inner.process.transport_live().await? {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    }).await.context("fixture child did not exit")?
}

#[test]
fn managed_close_drains_the_original_response_and_reaps_readers() -> Result<()> {
    runtime()?.block_on(async {
        let seat = OmpRpcSeat::spawn(python(r#"
import json, sys
print(json.dumps({'type':'ready','protocolVersion':1}), flush=True)
request = json.loads(sys.stdin.readline())
print(json.dumps({'type':'response','id':request['id'],'command':request['type'],
                  'success':False,'error':'fixture refusal'}), flush=True)
print('fixture stderr before exit', file=sys.stderr, flush=True)
"#), None).await?;
        let checked: Result<()> = async {
            seat.send_frame(&json!({"id":"original-request-17","type":"fixture"})).await?;
            process_exited(&seat).await
        }.await;
        // Teardown still runs if an earlier assertion/observation failed.
        let closed = seat.close().await;
        checked?;
        let receipt = closed?;
        ensure!(receipt.readers_joined == 2, "reader barrier: {receipt:?}");
        ensure!(!receipt.write_outcome_unknown && receipt.unknown_write.is_none());
        ensure!(!seat.inner.process.transport_live().await?);
        let expected = json!({"type":"response","id":"original-request-17",
            "command":"fixture","success":false,"error":"fixture refusal"});
        let events = seat.protocol_events().await;
        ensure!(events.iter().any(|event| matches!(event,
            OmpEvent::CommandResponse { response } if response == &expected)),
            "the original response id/result was lost: {events:?}");
        let refused = seat.send_frame(&json!({"id":"after-close","type":"fixture"})).await;
        let error = match refused {
            Err(error) => error,
            Ok(()) => anyhow::bail!("closed owner accepted another frame"),
        };
        let typed = error.downcast_ref::<ManagedWriteError>().context("missing write classification")?;
        ensure!(typed.state == ManagedWriteState::NotSent);
        ensure!(typed.request_id.as_deref() == Some("after-close"));
        ensure!(seat.protocol_events().await == events);
        let repeated = seat.close().await?;
        ensure!(repeated.process_id == receipt.process_id && repeated.readers_joined == 2);
        eprintln!("managed process close observation: {}", serde_json::to_string(&receipt)?);
        Ok(())
    })
}

struct WitnessDir(PathBuf);

impl WitnessDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("shenko-managed-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for WitnessDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn close_interrupts_a_partly_written_frame_and_keeps_its_unknown_identity() -> Result<()> {
    runtime()?.block_on(async {
        let witness = WitnessDir::new()?;
        let prefix_path = witness.0.join("prefix");
        let mut command = python(r#"
import pathlib, signal, sys
prefix = sys.stdin.buffer.read(1)
pathlib.Path(sys.argv[1]).write_bytes(prefix)
signal.pause()
"#);
        command.push(prefix_path.to_string_lossy().into_owned());
        let seat = OmpRpcSeat::spawn(command, None).await?;
        let frame = json!({"id":"original-blocked-write","type":"prompt","message":"x".repeat(8 * 1024 * 1024)});
        let mut wire = serde_json::to_vec(&frame)?;
        wire.push(b'\n');
        let digest = shenko_types::Digest::blake3(&wire);
        let checked: Result<_> = async {
            let mut write = Box::pin(seat.send_frame(&frame));
            // The witness proves that this exact child read a wire prefix;
            // elapsed time alone is not used as evidence of a partial write.
            tokio::time::timeout(WAIT, async {
                loop {
                    tokio::select! {
                        _result = &mut write => anyhow::bail!("fixture write unexpectedly completed: {_result:?}"),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {
                            match std::fs::read(&prefix_path) {
                                Ok(prefix) if prefix == b"{" => return Ok::<_, anyhow::Error>(()),
                                Ok(_) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                    }
                }
            }).await.context("fixture never observed the original wire prefix")??;
            let mut close = Box::pin(seat.close());
            let mut write_result = None;
            let mut close_result = None;
            tokio::time::timeout(Duration::from_secs(20), async {
                while write_result.is_none() || close_result.is_none() {
                    tokio::select! {
                        _result = &mut write, if write_result.is_none() => write_result = Some(_result),
                        _result = &mut close, if close_result.is_none() => close_result = Some(_result),
                    }
                }
            }).await.context("write/close did not reach their owned boundary")?;
            let error = match write_result.context("missing original write result")? {
                Err(error) => error,
                Ok(()) => anyhow::bail!("partial original write became a successful write"),
            };
            let typed = error.downcast_ref::<ManagedWriteError>().context("missing Unknown classification")?;
            ensure!(typed.state == ManagedWriteState::Unknown);
            ensure!(typed.request_id.as_deref() == Some("original-blocked-write"));
            ensure!(typed.wire_digest == digest);
            let receipt = close_result.context("missing close result")??;
            ensure!(receipt.write_outcome_unknown && receipt.readers_joined == 2);
            let unknown = receipt.unknown_write.as_ref().context("lost cancelled write identity")?;
            ensure!(unknown.request_id.as_deref() == Some("original-blocked-write") && unknown.wire_digest == digest);
            ensure!(!seat.inner.process.transport_live().await?);
            ensure!(seat.protocol_events().await.is_empty(), "fixture never sent a remote response");
            Ok(receipt)
        }.await;
        let teardown = seat.close().await;
        let receipt = checked?;
        teardown?;
        eprintln!("managed partial-write close observation: {}", serde_json::to_string(&receipt)?);
        Ok(())
    })
}

#[test]
fn dropping_the_last_adapter_does_not_leave_its_child_owned_by_readers() -> Result<()> {
    runtime()?.block_on(async {
        let seat = OmpRpcSeat::spawn(python("import signal; signal.pause()"), None).await?;
        let pid = seat.inner.process.process_id().context("spawned child has no PID")?;
        let owner = Arc::downgrade(&seat.inner);
        drop(seat);
        ensure!(owner.upgrade().is_none(), "stdout/stderr readers retained the process owner");
        tokio::time::timeout(WAIT, async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.context("abrupt owner loss did not remove the original process")?;
        eprintln!("managed abrupt-drop observation: original_pid={pid}, process_absent=true; no successful close receipt claimed");
        Ok(())
    })
}
