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
    subject_stopping: tokio_util::sync::CancellationToken,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut delivered = HashSet::new();
    // Durable results this process could not read. They stay durable and
    // undisposed; this only keeps one bad row from failing every pass.
    let mut unreadable = HashSet::new();
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
                        let ready = store.ready_native_result_rows(agent_id, 64)?;
                        let ready_ids: HashSet<_> =
                            ready.iter().map(|(result_id, _)| *result_id).collect();
                        delivered.retain(|id| ready_ids.contains(id));
                        for (result_id, data) in ready {
                            if !delivered.insert(result_id) {
                                continue;
                            }
                            // One unreadable durable result is an undecodable
                            // FACT: the original row keeps its identity and
                            // disposition, the skip is journaled once, and the
                            // remaining results still reach the subject.
                            let result: shenko_types::task::TaskResult =
                                match serde_json::from_str(&data) {
                                    Ok(result) => result,
                                    Err(error) => {
                                        if unreadable.insert(result_id) {
                                            store.journal_runtime_error(
                                                agent_id,
                                                shenko_store::RuntimeErrorContext {
                                                    kind: "undecodable_native_result".into(),
                                                    source_module: module_path!().into(),
                                                    message: format!(
                                                        "durable native result {result_id} is not a TaskResult this build reads: {error}"
                                                    ),
                                                    step_no: None,
                                                    task_id: None,
                                                    extra: serde_json::json!({
                                                        "result_id": result_id.to_string(),
                                                    }),
                                                },
                                            );
                                        }
                                        continue;
                                    }
                                };
                            if result.result_id != result_id {
                                // The row and its original disagree about which
                                // result this is. Delivering it would bind the
                                // wrong identity; leave it for reconciliation.
                                if unreadable.insert(result_id) {
                                    store.journal_runtime_error(
                                        agent_id,
                                        shenko_store::RuntimeErrorContext {
                                            kind: "native_result_identity_mismatch".into(),
                                            source_module: module_path!().into(),
                                            message: format!(
                                                "durable native result row {result_id} carries a different result identity"
                                            ),
                                            step_no: None,
                                            task_id: None,
                                            extra: serde_json::json!({
                                                "result_id": result_id.to_string(),
                                            }),
                                        },
                                    );
                                }
                                continue;
                            }
                            let event = InboxEvent {
                                event_id: result_id,
                                kind: InboxEventKind::TaskResult,
                                payload: serde_json::to_value(&result)?,
                                created_at: chrono::Utc::now(),
                                source: "native-child".into(),
                            };
                            // This is a volatile delivery projection: the
                            // original result remains durable on closure.
                            // Closure alone does not identify why the
                            // receiver stopped; it is not a success receipt.
                            if !deliver_native_result(
                                task_result_tx, event, &subject_stopping, &cancel,
                            ).await? {
                                break;
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

/// Deliver one durable native result into the subject inbox.
///
/// Stop delivery when the subject or dispatcher is stopping. Consumers may
/// still settle durable work after the subject stops receiving. An otherwise
/// closed inbox is an error, even though the original result remains durable.
async fn deliver_native_result(
    tx: &tokio::sync::mpsc::Sender<InboxEvent>,
    event: InboxEvent,
    subject_stopping: &tokio_util::sync::CancellationToken,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<bool> {
    tokio::select! { biased;
        _ = cancel.cancelled() => Ok(false),
        _ = subject_stopping.cancelled() => Ok(false),
        result = tx.send(event) => match result {
            Ok(()) => Ok(true),
            Err(_) if subject_stopping.is_cancelled() || cancel.is_cancelled() => Ok(false),
            Err(error) => Err(anyhow::Error::new(error)
                .context("subject result inbox closed before shutdown")),
        },
    }
}
