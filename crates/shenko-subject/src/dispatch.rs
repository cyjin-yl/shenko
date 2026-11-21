//! Consumers of committed S1 work. Broadcasts/channels are only projections;
//! durable ready indexes and per-event dispositions remain the recovery truth.
use crate::NativeChildService;
use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::inbox::{InboxEvent, InboxEventKind, SELF_WAKE_KIND, SUBJECT_WAKE_SOURCE};
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
    // One bounded due-page coordinate; durable inbox identity/disposition
    // replaces an ever-growing in-memory set of every wake ever produced.
    let mut wake_cursor = None;
    loop {
        tokio::select! { biased;
            _=cancel.cancelled()=>return Ok(()),
            // Retirement revoked new control admissions. Native workers and
            // their factual receipts keep their separate owned join path;
            // do not begin another control/publication settlement pass here.
            _=subject_stopping.cancelled()=>return Ok(()),
            _=tick.tick()=>{
                // One dispatcher pass. A fresh preparation displaced by
                // ordinary progress is re-read on a later tick; real failures
                // remain durable runtime facts and propagate.
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
                            let event = store.retain_inbox_event(agent_id, &event)?;
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
                            &store, agent_id, task_result_tx, &mut wake_cursor,
                        ).await?;
                    }
                    Ok(())
                }.await;
                if let Err(error) = pass {
                    // Capture saw a newer same-authority row BEFORE ordering
                    // this action. End this pass and re-read durable work on
                    // the normal tick; never retry a chosen image, an old
                    // permit, an external effect, or an arbitrary Conflict.
                    if error.downcast_ref::<shenko_types::error::ShenkoError>()
                        .is_some_and(|error| error.is_control_preparation_changed_for(agent_id))
                    {
                        tracing::debug!(%agent_id, %error, "fresh control preparation changed; re-evaluate durable work on the next dispatcher pass");
                        // A slow snapshot may have missed ticks. Do not
                        // burst through them as immediate preparation retries.
                        tick.reset();
                        continue;
                    }
                    // Graceful stop revokes new authority admissions while a
                    // dispatcher pass can still be finishing. That precise
                    // refusal leaves the work durable for reopen; another
                    // authority conflict or a storage failure remains an error.
                    if subject_stopping.is_cancelled()
                        && error.downcast_ref::<shenko_types::error::ShenkoError>()
                            .is_some_and(|error| error.is_runtime_admission_closed_for(store.capabilities().incarnation))
                    {
                        return Ok(());
                    }
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
    if cancel.is_cancelled() || subject_stopping.is_cancelled() { return Ok(false); }
    match tx.try_send(event) {
        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(true),
        Err(_) if subject_stopping.is_cancelled() || cancel.is_cancelled() => Ok(false),
        Err(error) => Err(anyhow::Error::new(error)
            .context("subject result inbox closed before shutdown")),
    }
}

pub(crate) const DUE_OBLIGATION_PAGE_SIZE: usize = 16;

/// Read one bounded due-obligation page per dispatcher pass and admit its
/// self-wakes for open obligations whose `due_at` has passed (
/// R071/R072). An obligation row is not a scheduler: this scanner is the
/// mechanism that turns the subject's own recorded promise into its own next
/// input, through the same durable inbox admission + sealed-step path every
/// other input takes.
///
/// Each pass advances at most one indexed page. A finite local row watermark
/// bounds a sweep even when new obligations keep arriving; the next sweep
/// starts from the beginning. Replay never re-sends an already admitted wake,
/// including an acknowledged one. Durable pending input owns lost delivery.
pub(crate) async fn wake_due_obligations(
    store: &Arc<Store>,
    agent_id: EntityId,
    inbox: &tokio::sync::mpsc::Sender<InboxEvent>,
    cursor: &mut Option<shenko_store::subject_state::DueObligationCursor>,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let page = store.due_subject_obligations_page(agent_id, now,
        cursor.as_ref(), DUE_OBLIGATION_PAGE_SIZE)?;
    for obligation in page.obligations {
        // SQLite's indexed timestamp has coarser precision than chrono. If
        // an instant rounded onto this tick, leave it for the next sweep.
        if obligation.due_at.is_none_or(|due| due > now) { continue; }
        let event = self_wake_event(&obligation);
        let admission = store.admit_inbox_event(
            event.event_id, agent_id, SELF_WAKE_KIND, SUBJECT_WAKE_SOURCE,
            &serde_json::to_value(&event)?,
        )?;
        if admission == shenko_store::InboxAdmission::Inserted {
            // A full/lost channel cannot lose the admitted original: ordinary
            // pending selection reads it. Replaying an existing row must never
            // manufacture another wake or undo its explicit S1 disposition.
            let _ = inbox.try_send(event);
        }
    }
    // Advance only after every admission succeeded. A partial pass can retry
    // its page safely because already-admitted identities return Replay.
    *cursor = page.next;
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
