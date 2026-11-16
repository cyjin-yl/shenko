//! The common durable publication consumer for every configured channel.
use std::sync::Arc;

use anyhow::Result;

use crate::runtime_channels::journal_channel_error;

pub(crate) fn spawn(
    channel_registry: &shenko_types::channel::ChannelRegistry,
    store: &Arc<shenko_store::Store>,
    agent_id: shenko_types::ids::EntityId,
    consumer_cancel: &tokio_util::sync::CancellationToken,
    cancel: &tokio_util::sync::CancellationToken,
) {
    // Outbound publications across every registered channel: the
    // model emits an explicit target so we never guess who to
    // answer. T1 (arm_publication) claims the intent and arms the
    // durable operation before the network is touched; the
    // provider's answer is E1 evidence. A target no installed
    // plugin serves is refused as never dispatched — durable,
    // effect none, and it never cancels the runtime.
    // docs/specs/channels.md#spec-35-6, docs/specs/durable-state.md#spec-41-4 X03, docs/specs/channels.md#spec-41-6.
    let reply_registry = channel_registry.clone();
    let reply_store = Arc::clone(store);
    let reply_agent = agent_id;
    let cancel_replies = consumer_cancel.clone();
    let reply_failure = cancel.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
        let mut tick=tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tokio::select! { biased;
                _ = cancel_replies.cancelled() => break,
                _ = tick.tick() => {}
            }
            // The ready index owns the work.
            for publication in reply_store.ready_publications(32)? {
                let intent_id = publication.intent_id;
                let Some(plugin) = reply_registry.channel(&publication.target.channel) else {
                    // Never dispatched: durable refusal, runtime
                    // keeps running.
                    if let Err(error) = reply_store.refuse_publication(
                        intent_id,
                        "no installed channel serves target.channel",
                    ) {
                        tracing::warn!(err=%error, %intent_id, "publication refusal not committed");
                    }
                    continue;
                };
                let dispatch = match plugin.describe(&publication.target) {
                    Ok(dispatch) => dispatch,
                    Err(error) => {
                        if let Err(refusal_error) =
                            reply_store.refuse_publication(intent_id, &error.to_string())
                        {
                            tracing::warn!(err=%refusal_error, %intent_id, "publication refusal not committed");
                        }
                        continue;
                    }
                };
                // T1: claim + arm before the network. None means
                // another dispatcher owns it — never a second send.
                let Some(armed) = reply_store.arm_publication(intent_id, &dispatch)? else {
                    // Not an error and not a send: either another
                    // writer owns the intent, or T1 committed that this
                    // reply can never be dispatched (cancelled reply,
                    // epoch advance, blocked authority). The committed
                    // task outcome distinguishes them; a never-dispatched
                    // reply becomes a subject-readable fact instead of a
                    // silent no-op, and the consumer keeps running.
                    project_never_dispatched_publication(
                        &reply_store,
                        intent_id,
                        &publication.target.channel,
                        &publication.target.account,
                    );
                    continue;
                };
                let mut dispatch = dispatch;
                dispatch.echo = Some(armed.operation_id.to_string());
                let outcome = match plugin.dispatch(&dispatch, &publication.content).await {
                    Ok(receipt) => {
                        reply_store.record_publication_receipt(&armed, &receipt)?;
                        if receipt.accepted { "accepted" } else { "rejected" }
                    }
                    Err(
                        error @ (shenko_types::channel::ChannelError::Transport { .. }
                        | shenko_types::channel::ChannelError::Unknown { .. }),
                    ) => {
                        // The request may have been executed. The
                        // effect is unknown; reconciliation
                        // settles it and never blind-resends.
                        let detail = error.to_string();
                        reply_store.record_publication_unknown(&armed, &detail)?;
                        "unknown"
                    }
                    Err(error) => {
                        // Deterministic refusal after arming
                        // (unsupported/unresolved/rejected): the
                        // effect is none and the operation is
                        // resolved failed.
                        let receipt = shenko_types::channel::ChannelReceipt {
                            status: "rejected".to_string(),
                            retcode: -1,
                            accepted: false,
                            provider_message_id: None,
                            raw: serde_json::json!({"error": error.to_string()}),
                            observed_at: chrono::Utc::now(),
                        };
                        reply_store.record_publication_receipt(&armed, &receipt)?;
                        "refused"
                    }
                };
                reply_store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                    channel: publication.target.channel.clone(),
                    account: armed.account.clone(),
                    kind: "publication_outcome".to_string(),
                    state: outcome.to_string(),
                    subject_ref: Some(intent_id.to_string()),
                    detail: serde_json::json!({
                        "operation_id": armed.operation_id.to_string(),
                        "kind": publication.target.kind,
                    }),
                })?;
            }
        }
        Ok(())
        }.await;
        if let Err(error) = result {
            journal_channel_error(
                &reply_store,
                reply_agent,
                "channel_publication_consumer_failed",
                "shenko::channel_publication_consumer",
                error.to_string(),
                serde_json::json!({}),
            );
            tracing::error!(err=%error, "publication consumer stopped; reopen writer and reconcile");
            reply_failure.cancel();
        }
    });

}

/// Project "this ready reply will never be dispatched" as a channel fact.
///
/// `Store::arm_publication` answers `Ok(None)` for two very different cases:
/// another writer already owns the intent, or T1 committed a terminal
/// `publication_never_dispatched` outcome because no attempt can exist
/// (cancelled reply, epoch advance, blocked authority). The second case is a
/// committed fact, not a transport failure: the subject may author a new reply
/// later. Reading the task's sealed outcome is the discriminator; every failure
/// here is only a missing projection, never a reason to stop the consumer or
/// retry the send.
fn project_never_dispatched_publication(
    store: &shenko_store::store::Store,
    intent_id: shenko_types::ids::EntityId,
    channel: &str,
    account: &str,
) {
    let task = match store.external_task(intent_id) {
        Ok(task) => task,
        Err(_) => return, // no task row: another writer owns it
    };
    let shenko_types::task::TaskState::Terminal { outcome_ref } = task.state else {
        return;
    };
    let bytes = match store.cas.read_object(&outcome_ref.digest) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(err=%error, %intent_id, "never-dispatched outcome unreadable");
            return;
        }
    };
    // The typed `ObjectRef` on the task outcome is the read authority: check the
    // length it declares before decoding. `subject_intents.result_ref` is opaque
    // per-writer text (a `file://` locator on this path) and is never parsed.
    if bytes.len() as u64 != outcome_ref.bytes {
        tracing::warn!(
            %intent_id,
            declared = outcome_ref.bytes,
            actual = bytes.len(),
            "never-dispatched outcome length mismatch"
        );
        return;
    }
    let outcome: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(err=%error, %intent_id, "never-dispatched outcome is not JSON");
            return;
        }
    };
    if outcome.get("kind").and_then(serde_json::Value::as_str)
        != Some("publication_never_dispatched")
    {
        return; // a settled/other terminal outcome is already its own fact
    }
    if let Err(error) = store.record_channel_fact(&shenko_store::channel::NewChannelFact {
        channel: channel.to_string(),
        account: account.to_string(),
        kind: "publication_outcome".to_string(),
        state: "never_dispatched".to_string(),
        subject_ref: Some(intent_id.to_string()),
        detail: serde_json::json!({
            "reason_kind": outcome.get("reason_kind"),
            "reason": outcome.get("reason"),
            "dispatched": outcome.get("dispatched"),
            "effect_status": outcome.get("effect_status"),
        }),
    }) {
        tracing::warn!(err=%error, %intent_id, "never-dispatched fact not recorded");
    }
}
