//! Consumers of committed S1 work. Broadcasts/channels are only projections;
//! durable ready indexes and per-event dispositions remain the recovery truth.
use crate::{InboxEvent, InboxEventKind, NativeChildService};
use shenko_store::Store;
use std::{collections::HashSet, sync::Arc};

pub async fn run_committed_dispatcher(
    store: Arc<Store>,
    native: Arc<NativeChildService>,
    agent_id: shenko_types::ids::EntityId,
    task_result_tx: Option<tokio::sync::mpsc::Sender<InboxEvent>>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut delivered = HashSet::new();
    loop {
        tokio::select! { biased;
            _=cancel.cancelled()=>return Ok(()),
            _=tick.tick()=>{
                // One dispatcher pass: every failure is a durable runtime
                // fact the subject can read (SPEC §45.14), then propagates.
                let pass: anyhow::Result<()> = async {
                    store.settle_subject_memory()?;
                    store.reconcile_subject_publications()?;
                    native.run_once().await?;

                    // With no subject model, results stay durable for the later
                    // subject to consume. Do not fill an undrained in-memory inbox.
                    if let Some(task_result_tx) = task_result_tx.as_ref() {
                        let ready = store.ready_native_results(agent_id, 64)?;
                        let ready_ids: HashSet<_> =
                            ready.iter().map(|result| result.result_id).collect();
                        delivered.retain(|id| ready_ids.contains(id));
                        for result in ready {
                            if !delivered.insert(result.result_id) {
                                continue;
                            }
                            let event = InboxEvent {
                                event_id: result.result_id,
                                kind: InboxEventKind::TaskResult,
                                payload: serde_json::to_value(&result)?,
                                created_at: chrono::Utc::now(),
                                source: "native-child".into(),
                            };
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                result = task_result_tx.send(event) => result?,
                            }
                        }
                    }
                    Ok(())
                }.await;
                if let Err(error) = pass {
                    store.journal_runtime_error(
                        agent_id,
                        shenko_store::RuntimeErrorContext {
                            kind: "native_dispatcher".into(),
                            source_module: module_path!().into(),
                            message: format!("committed dispatcher pass failed: {error:#}"),
                            step_no: None,
                            task_id: None,
                            extra: serde_json::json!({}),
                        },
                    );
                    return Err(error);
                }
            }
        }
    }
}
