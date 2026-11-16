//! NapCat connector, shared durable ingress and acknowledgement wiring.
use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use tracing::info;

use crate::runtime_channels::{journal_channel_error, prepare_channel_envelope};

pub(crate) struct NapCatOptions {
    pub(crate) napcat: Option<String>,
    pub(crate) napcat_account: Option<u64>,
    pub(crate) napcat_access_token: Option<String>,
    pub(crate) napcat_admin_qq: Vec<u64>,
    pub(crate) napcat_file_root: Option<std::path::PathBuf>,
    pub(crate) napcat_file_host: Option<String>,
    pub(crate) napcat_cli_url: Option<String>,
}

/// The composed NapCat installation inputs, grouped so the installer
/// signature states its work rather than the composition root's arity.
pub(super) struct NapCatInstall<'a> {
    pub(super) supervisor: &'a shenko_kernel::Supervisor,
    pub(super) store: &'a Arc<shenko_store::Store>,
    pub(super) data_dir: &'a Path,
    pub(super) agent_id: shenko_types::ids::EntityId,
    pub(super) subject_inbox: &'a Arc<shenko_subject::inbox::Inbox>,
    pub(super) channel_registry: &'a mut shenko_types::channel::ChannelRegistry,
    pub(super) consumer_cancel: &'a tokio_util::sync::CancellationToken,
    pub(super) options: NapCatOptions,
}

pub(super) async fn install(input: NapCatInstall<'_>) -> Result<Option<cordis_core::FiberHandle>> {
    let NapCatInstall {
        supervisor, store, data_dir, agent_id, subject_inbox,
        channel_registry, consumer_cancel, options,
    } = input;
    let NapCatOptions { napcat, napcat_account, napcat_access_token, napcat_admin_qq, napcat_file_root, napcat_file_host, napcat_cli_url } = options;
    // The connector-scoped definition stays owned by the composed channel services.
    let mut _native_contacts_plugin = None;
    // 5. Start NapCat if configured
    if napcat.is_some() || napcat_cli_url.is_some() {
        let napcat_config = shenko_channel_napcat::config::NapCatConfig {
            ws_url: napcat.clone().unwrap_or_default(),
            access_token: napcat_access_token,
            bot_qq: napcat_account.expect("NapCat account validated before startup"),
            admin_qq: napcat_admin_qq.clone(),
            file_root: napcat_file_root.clone(),
            file_host: napcat_file_host.clone(),
            ..Default::default()
        };
        // 独立接收域先落盘；打开失败停止启动，绝不降级到内存。
        let (napcat_client, mut napcat_event_rx, napcat_action_rx) =
            shenko_channel_napcat::client::NapCatClient::open_persistent(
                napcat_config.clone(),
                data_dir.join("napcat-ingress.sqlite"),
            )?;
        let client = Arc::new(napcat_client);
        let spool = client.spool.clone();
        let event_tx = client.event_sender();
        let pending_actions = client.pending_actions.clone();
        let cancel_napcat = consumer_cancel.clone();

        // Read-only contact/identity queries ride the same echo
        // protocol and durable task/evidence path as every native
        // child. Installing only while the connector exists keeps the
        // advertised capability honest: no NapCat, no definition.
        let contacts_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
            Arc::new(shenko_exec_host::NapCatContactsHandler::new(
                Arc::clone(store),
                Arc::clone(&client),
            ));
        _native_contacts_plugin = Some(
            supervisor
                .spawn(shenko_subject::NativeTaskHandlerPlugin, contacts_handler)
                .await?,
        );

        // The QQ plugin owns classification, reply resolution,
        // description, dispatch and attachment resolution; the Store
        // owns the durable intent, attempt, receipt and the sealing of
        // resolved attachment bytes. The plugin never holds a Store.
        let qq_plugin: Arc<shenko_channel_napcat::NapCatChannel> = Arc::new(
            shenko_channel_napcat::NapCatChannel::new(
                napcat_config.clone(),
                Arc::clone(&client),
            ),
        );
        channel_registry.register(qq_plugin.clone()).context("register qq channel plugin")?;
        // Administration is an owner-set fact, recorded at startup and
        // never derived from a group role or the transport token.
        for admin in &napcat_config.admin_qq {
            store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                channel: shenko_channel_napcat::QQ_CHANNEL_ID.to_string(),
                account: napcat_config.account_scope(),
                kind: "admin_grant".to_string(),
                state: "active".to_string(),
                subject_ref: Some(format!("qq-admin:{admin}")),
                detail: serde_json::json!({"admin_qq": admin, "source": "owner_config"}),
            })?;
        }
        store.record_channel_fact(&shenko_store::channel::NewChannelFact {
            channel: shenko_channel_napcat::QQ_CHANNEL_ID.to_string(),
            account: napcat_config.account_scope(),
            kind: "health".to_string(),
            state: "starting".to_string(),
            subject_ref: None,
            detail: serde_json::json!({
                "ws_configured": napcat.is_some(),
                "cli_source_configured": napcat_cli_url.is_some(),
            }),
        })?;

        // Connectivity is an observation, not a message: lifecycle and
        // heartbeat frames land in the spool's bounded meta ledger, and
        // a new frame is mirrored as a subject-readable fact. Staleness
        // stays visible because the fact carries the observation time.
        let connectivity_spool = Arc::clone(&spool);
        let connectivity_store = Arc::clone(store);
        let connectivity_account = napcat_config.account_scope();
        let connectivity_cancel = cancel_napcat.clone();
        tokio::spawn(async move {
            let mut last: Option<(String, Option<String>)> = None;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! { biased;
                    _ = connectivity_cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let meta = match connectivity_spool.last_meta() {
                    Ok(Some(meta)) => meta,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(err=%error, "NapCat connectivity observation unreadable");
                        break;
                    }
                };
                let signature = (meta.meta_event_type.clone(), meta.sub_type.clone());
                if last.as_ref() == Some(&signature) {
                    continue;
                }
                last = Some(signature);
                let state = match (meta.meta_event_type.as_str(), meta.sub_type.as_deref()) {
                    ("lifecycle", Some("enable" | "connect")) => "ready",
                    ("lifecycle", Some("disable")) => "stopped",
                    ("heartbeat", _) => "ready",
                    _ => "observed",
                };
                if let Err(error) = connectivity_store.record_channel_fact(
                    &shenko_store::channel::NewChannelFact {
                        channel: shenko_channel_napcat::QQ_CHANNEL_ID.to_string(),
                        account: connectivity_account.clone(),
                        kind: "connectivity".to_string(),
                        state: state.to_string(),
                        subject_ref: None,
                        detail: serde_json::json!({
                            "meta_event_type": meta.meta_event_type,
                            "sub_type": meta.sub_type,
                            "observed_at": meta.observed_at,
                        }),
                    },
                ) {
                    tracing::warn!(err=%error, "NapCat connectivity fact not recorded");
                }
            }
        });

        // A storage failure closes the connector and stops this runtime;
        // silently leaving an apparently healthy UI would hide lost reception.
        let connection_spool = Arc::clone(&spool);
        let connection_config = napcat_config.clone();
        let connection_cancel = cancel_napcat.clone();
        let connection_store = Arc::clone(store);
        let connection_agent = agent_id;
        if napcat.is_some() {
            tokio::spawn(async move {
                if let Err(error) =
                    shenko_channel_napcat::client::NapCatClient::run_connection_loop(
                        connection_config,
                        event_tx,
                        napcat_action_rx,
                        pending_actions,
                        connection_spool,
                        connection_cancel.clone(),
                    )
                    .await
                {
                    // The failure is a durable fact of the subject's
                    // history, not just a stderr line: journal it with
                    // the actual agent identity before stopping.
                    journal_channel_error(
                        &connection_store,
                        connection_agent,
                        "napcat_ingress_failed",
                        "shenko_channel_napcat::client",
                        error.to_string(),
                        serde_json::json!({}),
                    );
                    tracing::error!(err=%error, "NapCat ingress failed — stopping runtime for recovery");
                    connection_cancel.cancel();
                }
            });
        }

        // napcat-cli is a second SOURCE of the same QQ plugin: its
        // events enter the same spool, dedup by the same source
        // identity, and are classified by the same plugin.
        if let Some(cli_url) = napcat_cli_url.clone() {
            let cli_spool = Arc::clone(&spool);
            let cli_events = client.event_sender();
            let cli_cancel = cancel_napcat.clone();
            let cli_store = Arc::clone(store);
            let cli_agent = agent_id;
            tokio::spawn(async move {
                if let Err(error) = shenko_channel_napcat::NapCatCliSource::new(
                    cli_url,
                    napcat_config.bot_qq,
                )
                .run(cli_spool, cli_events, cli_cancel.clone())
                .await
                {
                    journal_channel_error(
                        &cli_store,
                        cli_agent,
                        "napcat_cli_source_failed",
                        "shenko_channel_napcat::napcat_cli",
                        error.to_string(),
                        serde_json::json!({}),
                    );
                    tracing::error!(err=%error, "napcat-cli source failed — stopping runtime for recovery");
                    cli_cancel.cancel();
                }
            });
        }

        // Keep the ingress identity across replay. L's committed S1 receipt
        // wins over a stale I acknowledgement mirror after a crash.
        let inbox_tx = subject_inbox.sender();
        let event_store = Arc::clone(store);
        let event_spool = Arc::clone(&spool);
        let admission_plugin = Arc::clone(&qq_plugin);
        let cancel_events = cancel_napcat.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                loop {
                    let ev = tokio::select! {
                        ev = napcat_event_rx.recv() => match ev {
                            Some(ev) => ev,
                            None => break,
                        },
                        _ = cancel_events.cancelled() => break,
                    };
                    let plugin: Arc<dyn shenko_types::channel::ChannelPlugin> =
                        admission_plugin.clone();
                    let Some(inbox_ev) = napcat_inbox_event(
                        &event_store, agent_id, &event_spool, &plugin, ev,
                    ).await? else { continue; };
                    tokio::select! { biased;
                        _ = cancel_events.cancelled() => break,
                        sent = inbox_tx.send(inbox_ev) => {
                            if let Err(error) = sent {
                                // The spool retains the event, but a
                                // lost live receiver is still a fault.
                                if cancel_events.is_cancelled() { break; }
                                return Err(anyhow::Error::new(error)
                                    .context("subject channel inbox closed before shutdown"));
                            }
                        }
                    }
                }
                Ok(())
            }.await;
            if let Err(error) = result {
                journal_channel_error(
                    &event_store,
                    agent_id,
                    "napcat_admission_failed",
                    "shenko::channel_admission",
                    error.to_string(),
                    serde_json::json!({}),
                );
                tracing::error!(err=%error, "NapCat admission failed — ingress remains pending");
                cancel_events.cancel();
            }
        });

        // Poll bounded pages: cross-node watchers and one-shot callbacks
        // are not recovery truth. No S1 receipt means no ACK, even if delivered.
        let ack_store = Arc::clone(store);
        let cancel_acks = cancel_napcat.clone();
        let ack_agent = agent_id;
        tokio::spawn(async move {
            let result: Result<()> = async {
                let mut after = 0;
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {},
                        _ = cancel_acks.cancelled() => break,
                    }
                    let page = spool.pending_after(after, 256)?;
                    if page.is_empty() {
                        after = 0;
                    }
                    for event in page {
                        after = event.seq;
                        if let Some(commit) =
                            ack_store.event_disposition_commit(agent_id, event.spool_id)?
                        {
                            spool.ack(event.spool_id, &commit.to_string())?;
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                journal_channel_error(
                    &ack_store,
                    ack_agent,
                    "napcat_ack_mirror_failed",
                    "shenko::channel_ack_mirror",
                    error.to_string(),
                    serde_json::json!({}),
                );
                tracing::error!(err=%error, "NapCat acknowledgement mirror failed — reopen required");
                cancel_acks.cancel();
            }
        });

        info!("NapCat connector started");
    }

    Ok(_native_contacts_plugin)
}

/// Reuse the first canonical original before applying today's classifier.
/// The spool id belongs to the account-bound ingress ledger; the Store read
/// additionally confines the retained input to this agent's current branch.
/// Neither normalization upgrades nor attachment refreshes can replace that
/// original or the projection already staged alongside it. Only an S1
/// disposition allows acknowledgement and suppresses another delivery.
pub(crate) async fn napcat_inbox_event(
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    spool: &shenko_channel_napcat::IngressSpool,
    plugin: &Arc<dyn shenko_types::channel::ChannelPlugin>,
    event: shenko_channel_napcat::client::OneBotEvent,
) -> Result<Option<shenko_types::inbox::InboxEvent>> {
    let event_id = event.ingress_id
        .ok_or_else(|| anyhow::anyhow!("NapCat event missing durable ingress identity"))?;
    let received = spool.event(event_id)?;
    if let Some(commit) = store.event_disposition_commit(agent_id, event_id)? {
        spool.ack(event_id, &commit.to_string())?;
        return Ok(None);
    }
    if let Some(staged) = store.first_subject_input(agent_id, event_id)? {
        let mut data: serde_json::Value = serde_json::from_str(&staged.data)?;
        let original = data.get_mut("inbox_event").map(serde_json::Value::take)
            .ok_or_else(|| anyhow::anyhow!("staged original inbox event missing"))?;
        let original: shenko_types::inbox::InboxEvent = serde_json::from_value(original)?;
        anyhow::ensure!(original.event_id == event_id
            && original.kind == shenko_types::inbox::InboxEventKind::Message
            && original.source == shenko_channel_napcat::QQ_SOURCE,
            "NapCat retained input source identity mismatch");
        if original.payload.get("provider").and_then(serde_json::Value::as_str) == Some(plugin.channel_id())
            && original.payload.get("schema_version").and_then(serde_json::Value::as_u64) == Some(1)
        {
            let envelope: shenko_types::channel::ChannelEnvelope =
                serde_json::from_value(original.payload.clone())?;
            anyhow::ensure!(envelope.provider == plugin.channel_id()
                && envelope.account == plugin.account_scope()
                && envelope.transport_identity_domain == shenko_channel_napcat::QQ_TRANSPORT_IDENTITY_DOMAIN,
                "NapCat retained input account scope mismatch");
        }
        return Ok(Some(original));
    }

    // Re-read the first wire original from the spool, never a later transport
    // observation. Conflict/revision evidence stays in that ledger.
    let mut original = shenko_types::inbox::InboxEvent {
        event_id,
        kind: shenko_types::inbox::InboxEventKind::Message,
        payload: received.raw,
        created_at: received.received_at,
        source: shenko_channel_napcat::QQ_SOURCE.to_string(),
    };
    if let Some(mut envelope) = plugin.classify(&original.source, &original.payload) {
        envelope.received_at = original.created_at;
        envelope = prepare_channel_envelope(plugin, store, event_id, envelope).await?;
        original.payload = serde_json::to_value(&envelope)?;
    }
    Ok(Some(original))
}
