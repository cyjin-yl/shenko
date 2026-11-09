//! Consumers of committed S1 work. Broadcasts/channels are only projections;
//! durable ready indexes and per-event dispositions remain the recovery truth.
use crate::{InboxEvent, InboxEventKind, NativeChildService};
use crate::inbox::{SELF_WAKE_KIND, SUBJECT_WAKE_SOURCE};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::subject::PendingObligation;
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
    // Obligation wakes this process already produced, keyed on the exact due
    // fact: the same obligation redated to a new due_at is a new wake. The
    // durable admission and the S1 disposition are the cross-process truth;
    // this only bounds how often one running dispatcher scans its own sends.
    let mut wake_sent: HashSet<(EntityId, chrono::DateTime<chrono::Utc>)> = HashSet::new();
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

                        // Proactive wake (R071/R072): a durable obligation
                        // whose due_at has passed makes the subject produce
                        // its own next input instead of waiting for an
                        // external event to poke it. The admitted row is the
                        // fact; this channel send is only a volatile wake.
                        wake_due_obligations(
                            &store, agent_id, task_result_tx, &mut wake_sent,
                        ).await?;
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

/// Scan the durable obligation ledger once per dispatcher pass and admit a
/// self-wake for every open obligation whose `due_at` has passed (SPEC §13.8;
/// R071/R072). An obligation row is not a scheduler: this scanner is the
/// mechanism that turns the subject's own recorded promise into its own next
/// input, through the same durable inbox admission + sealed-step path every
/// other input takes.
///
/// `sent` bounds repeated emissions inside this process: a `(obligation_id,
/// due_at)` key already produced is not admitted again. After a restart the
/// deterministic event id makes the re-admission an `InboxAdmission::Replay`,
/// so a still-open obligation wakes exactly once per due fact across process
/// boundaries, never a duplicate row.
pub(crate) async fn wake_due_obligations(
    store: &Arc<Store>,
    agent_id: EntityId,
    inbox: &tokio::sync::mpsc::Sender<InboxEvent>,
    sent: &mut HashSet<(EntityId, chrono::DateTime<chrono::Utc>)>,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    // The obligation's own projection already parses due_at back into an
    // instant; a row this build cannot read fails the whole pass rather
    // than silently skipping one of the subject's promises.
    let due: Vec<PendingObligation> = store
        .open_subject_obligations(agent_id)?
        .into_iter()
        .filter(|obligation| obligation.due_at.is_some_and(|due| due <= now))
        .collect();
    sent.retain(|(obligation_id, due_at)| {
        due.iter().any(|o| o.obligation_id == *obligation_id && o.due_at == Some(*due_at))
    });
    for obligation in due {
        let key = (obligation.obligation_id, obligation.due_at.expect("filtered above"));
        if sent.contains(&key) {
            continue;
        }
        let event = self_wake_event(&obligation);
        // Admission is the durable fact: an identical fact under the same
        // deterministic identity is a replay, and a divergent one is a
        // conflict — never a second wake row.
        store.admit_inbox_event(
            event.event_id,
            agent_id,
            SELF_WAKE_KIND,
            SUBJECT_WAKE_SOURCE,
            &serde_json::to_value(&event)?,
        )?;
        sent.insert(key);
        // A full or lost channel does not drop the wake: pending_inbox_events
        // drains the durable row by source on the next step.
        let _ = inbox.try_send(event);
    }
    Ok(())
}

/// The deterministic wake for one due obligation. The event identity names
/// the due fact itself — obligation and its due instant — so a restart, a
/// replayed admission or a second dispatcher all derive the same event.
/// `created_at` is the obligation's due instant, not the scan time: a later
/// process reproduces the identical original instead of conflicting with
/// the admitted row under a fresh timestamp.
pub(crate) fn self_wake_event(obligation: &PendingObligation) -> InboxEvent {
    let due = obligation.due_at.expect("a wake requires a due obligation");
    InboxEvent {
        event_id: stable_wake_id(obligation.obligation_id, due),
        kind: InboxEventKind::SelfWake,
        payload: serde_json::json!({
            "kind": "obligation_due",
            "obligation_id": obligation.obligation_id,
            "due_at": due.to_rfc3339(),
            "description": &obligation.description,
            "resume_condition": &obligation.resume_condition,
            "depends_on_task_id": obligation.depends_on_task_id,
            "text": format!(
                "Your obligation {} is due ({}): {} — resume condition: {}",
                obligation.obligation_id,
                due.to_rfc3339(),
                obligation.description,
                obligation.resume_condition
            ),
        }),
        created_at: due,
        source: SUBJECT_WAKE_SOURCE.into(),
    }
}

/// A stable event id for one due fact. The same obligation at the same due
/// instant always produces the same id; a redated obligation is a different
/// due fact and produces a different wake.
fn stable_wake_id(obligation_id: EntityId, due: chrono::DateTime<chrono::Utc>) -> EntityId {
    let identity = format!("subject-obligation-due/v1:{obligation_id}:{}", due.to_rfc3339());
    let hash = blake3::hash(identity.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityId(uuid::Uuid::from_bytes(bytes))
}
