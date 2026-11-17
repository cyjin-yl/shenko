//! Configured channel adapters and their durable ingress-to-subject delivery.
use std::{path::{Path, PathBuf}, sync::Arc};

use anyhow::Result;
use tracing::info;

use crate::runtime_channels::{journal_channel_error, prepare_channel_envelope};

pub(super) async fn install(
    store: &Arc<shenko_store::Store>,
    data_dir: &Path,
    agent_id: shenko_types::ids::EntityId,
    subject_inbox: &Arc<shenko_subject::inbox::Inbox>,
    channel_registry: &mut shenko_types::channel::ChannelRegistry,
    consumer_cancel: &tokio_util::sync::CancellationToken,
    channels_config: Option<PathBuf>,
) -> Result<()> {
    // Additional channel connectors (official QQ, WeChat iLink,
    // Feishu/Lark) are installed only when the operator names their
    // accounts in one configuration file. An absent platform is not
    // registered, not advertised in the reply grammar and never
    // contacted. Capability discovery and health are recorded as
    // subject-readable facts, and administration stays an owner-set
    // fact per platform.
    if let Some(config_path) = channels_config.clone() {
        let file = shenko_channel_adapters::AdapterConfigFile::load(&config_path)
            .map_err(|error| anyhow::anyhow!(error))?;
        let adapters = shenko_channel_adapters::register_adapters(
            channel_registry,
            file.into_deps(data_dir.to_path_buf()),
        )
        .map_err(|error| anyhow::anyhow!(error))?;
        for observation in adapters.observations() {
            let health = observation.health.snapshot().await;
            store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                channel: observation.channel.clone(),
                account: observation.account.clone(),
                kind: "health".to_string(),
                state: health.state.clone(),
                subject_ref: None,
                detail: serde_json::json!({
                    "source": observation.source,
                    "detail": health.detail,
                }),
            })?;
        }
        if let Some(plugin) = &adapters.qq {
            record_owner_set_admins(
                store,
                shenko_channel_adapters::qq_official::events::QQ_OFFICIAL_CHANNEL_ID,
                &plugin.config().account_scope(),
                &plugin.config().admin_openids,
                "qq-official-admin",
                "open_id",
            )?;
        }
        if let Some(plugin) = &adapters.ilink {
            record_owner_set_admins(
                store,
                shenko_channel_adapters::ilink::events::ILINK_CHANNEL_ID,
                &plugin.config().account_scope(),
                &plugin.config().admin_user_ids,
                "ilink-admin",
                "user_id",
            )?;
        }
        if let Some(plugin) = &adapters.feishu {
            record_owner_set_admins(
                store,
                shenko_channel_adapters::feishu::events::FEISHU_CHANNEL_ID,
                &plugin.config().account_scope(),
                &plugin.config().admin_open_ids,
                "feishu-admin",
                "open_id",
            )?;
        }

        // Every monitor records into its own durable ledger before it
        // advances the platform cursor; these loops admit what the
        // ledger holds and acknowledge only after the subject's
        // disposition commit. One platform's connector failing never
        // stops the subject loop.
        let adapter_cancel = consumer_cancel.clone();
        let adapter_agent = agent_id;
        for handle in adapters.run_ingress(adapter_cancel.clone()).await {
            let failure_store = Arc::clone(store);
            tokio::spawn(async move {
                if let Ok(Err(error)) = handle.await {
                    journal_channel_error(
                        &failure_store,
                        adapter_agent,
                        "channel_ingress_failed",
                        "shenko_channel_adapters",
                        error,
                        serde_json::json!({}),
                    );
                    tracing::error!("channel monitor stopped; its durable ingress stays pending");
                }
            });
        }
        for ledger in adapters.ingress().iter().cloned() {
            let delivery_store = Arc::clone(store);
            let failure_store = Arc::clone(store);
            let delivery_registry = channel_registry.clone();
            let delivery_inbox = subject_inbox.sender();
            let delivery_cancel = adapter_cancel.clone();
            tokio::spawn(async move {
                if let Err(error) = run_adapter_ingress(
                    ledger,
                    delivery_registry,
                    delivery_store,
                    adapter_agent,
                    delivery_inbox,
                    delivery_cancel,
                )
                .await
                {
                    journal_channel_error(
                        &failure_store,
                        adapter_agent,
                        "channel_ingress_admission_failed",
                        "shenko::channel_admission",
                        error.to_string(),
                        serde_json::json!({}),
                    );
                    tracing::error!(err=%error, "adapter ingress delivery stopped; events stay pending");
                }
            });
        }
        // Health is a live observation, so a state change is written
        // as a new fact. The startup fact above already names the
        // initial state; this loop only records real transitions.
        let health_observations = adapters.observations().to_vec();
        let health_store = Arc::clone(store);
        let health_cancel = adapter_cancel.clone();
        tokio::spawn(async move {
            let mut seen: Vec<Option<(String, Option<String>, Option<chrono::DateTime<chrono::Utc>>)>> =
                vec![None; health_observations.len()];
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! { biased;
                    _ = health_cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                for (index, observation) in health_observations.iter().enumerate() {
                    let health = observation.health.snapshot().await;
                    let signature =
                        (health.state.clone(), health.detail.clone(), health.last_event_at);
                    if seen[index].as_ref() == Some(&signature) {
                        continue;
                    }
                    let first = seen[index].is_none();
                    seen[index] = Some(signature);
                    if first {
                        // The startup fact already recorded this state.
                        continue;
                    }
                    if let Err(error) = health_store.record_channel_fact(
                        &shenko_store::channel::NewChannelFact {
                            channel: observation.channel.clone(),
                            account: observation.account.clone(),
                            kind: "health".to_string(),
                            state: health.state.clone(),
                            subject_ref: None,
                            detail: serde_json::json!({
                                "source": observation.source,
                                "detail": health.detail,
                            }),
                        },
                    ) {
                        tracing::warn!(err=%error, channel=%observation.channel, "channel health fact not recorded");
                    }
                }
            }
        });
        info!(
            adapters = adapters.observations().len(),
            "channel adapters installed"
        );
    }

    Ok(())
}

/// Admit one channel event from a plugin-owned source (napcat-cli poller or
/// an adapter ingress). The event id is derived from the stable provider
/// keys (source, provider, account, platform message id, revision), so an
/// at-least-once redelivery of the same external event meets the committed
/// disposition and is admitted exactly once.
async fn admit_channel_event(
    registry: &shenko_types::channel::ChannelRegistry,
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    inbox_tx: &tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>,
    source: &str,
    mut envelope: shenko_types::channel::ChannelEnvelope,
    raw: &serde_json::Value,
) -> Result<()> {
    // The connector's own durable ingress ledger derived this same id from
    // the same replay-stable key, so a redelivery meets this check.
    let event_id = shenko_types::channel::envelope_event_id(source, &envelope);
    if store
        .event_disposition_commit(agent_id, event_id)?
        .is_some()
    {
        return Ok(());
    }
    if let Some(original) = store.inbox_event_original(agent_id, event_id)? {
        anyhow::ensure!(original.source == source && original.kind == shenko_types::inbox::InboxEventKind::Message,
            "retained channel input owner mismatch");
        let _ = inbox_tx.try_send(original);
        return Ok(());
    }
    let Some(plugin) = registry.channel_for_source(source) else {
        anyhow::bail!("no registered channel plugin owns source {source}");
    };
    envelope = prepare_channel_envelope(plugin, store, event_id, envelope).await?;
    let inbox_ev = shenko_types::inbox::InboxEvent {
        event_id,
        kind: shenko_types::inbox::InboxEventKind::Message,
        payload: serde_json::to_value(&envelope)?,
        created_at: envelope.received_at,
        source: source.to_string(),
    };
    let _ = raw; // the raw frame stays in the plugin's own ingress ledger
    let inbox_ev = store.retain_inbox_event(agent_id, &inbox_ev)?;
    // The original is durable; a busy or blocked subject cannot backpressure
    // the channel's own acquisition/ACK loop through its volatile wake buffer.
    let _ = inbox_tx.try_send(inbox_ev);
    Ok(())
}

/// Drain one adapter's durable ingress ledger into subject admission.
///
/// The ledger is the connector's own durable record: a monitor wrote the event
/// before it advanced the platform cursor, so this loop may replay after a
/// restart. Admission is idempotent on the shared envelope identity, and the
/// row is acknowledged only once the subject's disposition commit exists — a
/// delivery is never an acknowledgement.
async fn run_adapter_ingress(
    ingress: shenko_channel_adapters::SharedAdapterIngress,
    registry: shenko_types::channel::ChannelRegistry,
    store: Arc<shenko_store::store::Store>,
    agent_id: shenko_types::ids::EntityId,
    inbox_tx: tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // Two independent watermarks, mirroring the shipped NapCat ingress: the
    // delivery scan only moves forward in this process (a restart replays what
    // is still unacknowledged exactly once), and the acknowledgement mirror
    // re-reads from the beginning because a disposition may commit long after
    // the event was delivered.
    let mut deliver_after = 0;
    let mut ack_after = 0;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        tokio::select! { biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        for event in ingress
            .unacked(deliver_after, 64)
            .map_err(|error| anyhow::anyhow!(error))?
        {
            deliver_after = event.seq;
            if store.event_disposition_commit(agent_id, event.event_id)?.is_some() {
                continue;
            }
            admit_channel_event(
                &registry,
                &store,
                agent_id,
                &inbox_tx,
                &event.source,
                event.envelope,
                &event.raw,
            )
            .await?;
            ingress
                .mark_delivered(event.event_id)
                .map_err(|error| anyhow::anyhow!(error))?;
        }
        let mirror = ingress
            .unacked(ack_after, 64)
            .map_err(|error| anyhow::anyhow!(error))?;
        if mirror.is_empty() {
            ack_after = 0;
            continue;
        }
        for event in mirror {
            ack_after = event.seq;
            if let Some(commit) = store.event_disposition_commit(agent_id, event.event_id)? {
                ingress
                    .mark_delivered(event.event_id)
                    .map_err(|error| anyhow::anyhow!(error))?;
                ingress
                    .ack(event.event_id, &commit.to_string())
                    .map_err(|error| anyhow::anyhow!(error))?;
            }
        }
    }
}

/// Record owner-set administrators as channel facts.
///
/// Administration is never derived from a platform role, an app ticket or
/// familiarity: it exists only because the owner named the account in the
/// channel configuration, and the fact says so.
fn record_owner_set_admins(
    store: &shenko_store::store::Store,
    channel: &str,
    account: &str,
    admins: &[String],
    grant_prefix: &str,
    id_field: &str,
) -> Result<()> {
    for admin in admins {
        if admin.trim().is_empty() {
            anyhow::bail!("{channel} administrator id must not be empty");
        }
        store.record_channel_fact(&shenko_store::channel::NewChannelFact {
            channel: channel.to_string(),
            account: account.to_string(),
            kind: "admin_grant".to_string(),
            state: "active".to_string(),
            subject_ref: Some(format!("{grant_prefix}:{admin}")),
            detail: serde_json::json!({ id_field: admin, "source": "owner_config" }),
        })?;
    }
    Ok(())
}
