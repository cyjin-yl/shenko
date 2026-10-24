//! Shenko — personal agent framework entry point.
//! SPEC §44: seed/bootstrap startup with real subject loop + broker.
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "shenko", about = "Shenko（深刻）— personal agent framework", version = "0.1.0")]
struct Cli {
    #[arg(long, env = "SHENKO_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value = "info", env = "SHENKO_LOG")]
    log: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start Shenko agent with UI and continuous thought loop
    Start {
        #[arg(long, default_value = "7860")]
        port: u16,
        #[arg(long)]
        name: Option<String>,
        /// NapCat WebSocket URL (e.g. ws://127.0.0.1:3001)
        #[arg(long, env = "NAPCAT_WS_URL")]
        napcat: Option<String>,
        /// Account bound to the persistent ingress database (not an admin grant).
        #[arg(long, env = "NAPCAT_BOT_QQ", requires = "napcat")]
        napcat_account: Option<u64>,
    },
    /// Validate configuration and exit
    Check,
    /// Print version info
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_target(true)
        .compact()
        .init();

    let data_dir = cli.data_dir.unwrap_or_else(|| {
        dirs_next::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("shenko")
    });

    match cli.command {
        Command::Version => {
            println!("Shenko（深刻）0.1.0  SPEC v0.6.2");
            println!("Model: claude-opus-5-5");
            println!("ANTHROPIC_API_KEY: {}", if std::env::var("ANTHROPIC_API_KEY").is_ok() { "set" } else { "not set (stub mode)" });
        }

        Command::Check => {
            info!(data_dir=%data_dir.display(), "check");
            tokio::fs::create_dir_all(&data_dir).await?;
            let db_path = data_dir.join("shenko.db");
            let cas_dir = data_dir.join("cas");
            let store = shenko_store::store::Store::open(db_path, cas_dir)?;
            println!("Store: OK (SQLite {}, WAL={}, FTS5={})",
                store.capabilities.sqlite_version,
                store.capabilities.wal_mode,
                store.capabilities.fts5_available,
            );
            println!("API key: {}", if std::env::var("ANTHROPIC_API_KEY").is_ok() { "present" } else { "absent — stub mode" });
            println!("OK");
        }

        Command::Start { port, name, napcat, napcat_account } => {
            if napcat.is_some() {
                anyhow::ensure!(napcat_account.is_some_and(|account| account > 0),
                    "NapCat persistent ingress requires --napcat-account or NAPCAT_BOT_QQ");
            }
            info!(
                data_dir=%data_dir.display(), port,
                name=name.as_deref().unwrap_or("(unnamed)"),
                "Shenko starting"
            );

            // 1. Open persistent store
            tokio::fs::create_dir_all(&data_dir).await?;
            let db_path = data_dir.join("shenko.db");
            let cas_dir = data_dir.join("cas");
            let store = Arc::new(shenko_store::store::Store::open(db_path, cas_dir)?);
            info!(
                sqlite=%store.capabilities.sqlite_version,
                wal=store.capabilities.wal_mode,
                fts5=store.capabilities.fts5_available,
                "store ready"
            );

            // 2. Bootstrap genesis if first run — SPEC §44.6
            let agent_id = bootstrap_genesis(&store, name.as_deref())?;

            // 3. Start inference broker
            let (broker, broker_handle) = shenko_broker::broker::Broker::new();
            tokio::spawn(broker.run());

            // 4. Start subject loop — the continuous main agent. SPEC §5.1
            let cancel = tokio_util::sync::CancellationToken::new();
            let subject_inbox = Arc::new(shenko_subject::inbox::Inbox::new(1024));
            let (dispatcher_inner, mut dispatch_rx) =
                shenko_subject::dispatch::ChildDispatcher::new(agent_id);
            let dispatcher = Arc::new(dispatcher_inner);

            // Delegation consumer — the child side of the dispatcher. SPEC §5.2/§5.3.
            // Delegated intents used to be dropped together with the receiver; now
            // each one is durably recorded as a Pending TaskRecord so a committed
            // decision is never silently lost.
            let dispatch_store = Arc::clone(&store);
            let dispatch_cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    let received = tokio::select! {
                        received = dispatch_rx.recv() => received,
                        _ = dispatch_cancel.cancelled() => break,
                    };
                    let Some((task_id, intent)) = received else { break };
                    let kind = "delegation";
                    match persist_delegation(&dispatch_store, agent_id, task_id, &intent, kind) {
                        Ok(commit) => info!(
                            task_id = %task_id,
                            intent_id = %intent.intent_id,
                            kind,
                            commit = %commit,
                            goal = %intent.goal,
                            "delegation received — child task recorded (Pending)"
                        ),
                        Err(e) => tracing::error!(
                            task_id = %task_id,
                            intent_id = %intent.intent_id,
                            kind,
                            err = %e,
                            "delegation received but could not be recorded — intent not executed"
                        ),
                    }
                    // TODO(§35.4): seat execution (BuildProvider / AgentSeat)
                    // attaches here. This round records the delegated intent; it
                    // does not run a coding agent or advance the task state.
                }
            });
            info!("delegation consumer started");
            let hook_registry = Arc::new(shenko_kernel::hooks::HookRegistry::new());
            // Outbound path: subject model output → UI timeline. SPEC §5.1, §21.12
            let (outbox_tx, _outbox_rx) =
                tokio::sync::broadcast::channel::<shenko_subject::outbox::OutboxMessage>(64);
            let loop_state = shenko_subject::loop_runner::SubjectLoopState::new(
                agent_id,
                Arc::clone(&subject_inbox),
                Arc::clone(&dispatcher),
                Arc::clone(&store),
            )?
            .with_broker(broker_handle)
            .with_hooks(Arc::clone(&hook_registry))
            .with_outbox(outbox_tx.clone());

            let cancel_subject = cancel.clone();
            tokio::spawn(shenko_subject::loop_runner::run_subject_loop(loop_state, cancel_subject));
            info!("subject loop started");

            // 5. Start NapCat if configured — SPEC §16, §35.6
            if let Some(ws_url) = napcat {
                use shenko_channel_napcat::outbound::{
                    OutboundKind, OutboundLedger, OutboundOperation,
                };

                let napcat_config = shenko_channel_napcat::config::NapCatConfig {
                    ws_url,
                    bot_qq: napcat_account.expect("NapCat account validated before startup"),
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
                let cancel_napcat = cancel.clone();

                // A storage failure closes the connector and stops this runtime;
                // silently leaving an apparently healthy UI would hide lost reception.
                let connection_spool = Arc::clone(&spool);
                let connection_config = napcat_config.clone();
                let connection_cancel = cancel_napcat.clone();
                tokio::spawn(async move {
                    if let Err(error) = shenko_channel_napcat::client::NapCatClient::run_connection_loop(
                        connection_config, event_tx, napcat_action_rx, pending_actions,
                        connection_spool, connection_cancel.clone(),
                    ).await {
                        tracing::error!(err=%error, "NapCat ingress failed — stopping runtime for recovery");
                        connection_cancel.cancel();
                    }
                });

                // Keep the ingress identity across replay. L's committed S1 receipt
                // wins over a stale I acknowledgement mirror after a crash.
                let inbox_tx = subject_inbox.sender();
                let event_store = Arc::clone(&store);
                let event_spool = Arc::clone(&spool);
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
                            let event_id = ev.ingress_id.ok_or_else(|| anyhow::anyhow!("NapCat event missing durable ingress identity"))?;
                            if let Some(commit) = event_store.event_disposition_commit(agent_id, event_id)? {
                                event_spool.ack(event_id, &commit.to_string())?;
                                continue;
                            }
                            let inbox_ev = napcat_inbox_event(ev)?;
                            tokio::select! {
                                sent = inbox_tx.send(inbox_ev) => sent.map_err(|_| anyhow::anyhow!("subject inbox closed; ingress remains pending"))?,
                                _ = cancel_events.cancelled() => break,
                            }
                        }
                        Ok(())
                    }.await;
                    if let Err(error) = result {
                        tracing::error!(err=%error, "NapCat admission failed — ingress remains pending");
                        cancel_events.cancel();
                    }
                });

                // Poll bounded pages: cross-node watchers and one-shot callbacks
                // are not recovery truth. No S1 receipt means no ACK, even if delivered.
                let ack_store = Arc::clone(&store);
                let cancel_acks = cancel_napcat.clone();
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
                            if page.is_empty() { after = 0; }
                            for event in page {
                                after = event.seq;
                                if let Some(commit) = ack_store.event_disposition_commit(agent_id, event.spool_id)? {
                                    spool.ack(event.spool_id, &commit.to_string())?;
                                }
                            }
                        }
                        Ok(())
                    }.await;
                    if let Err(error) = result {
                        tracing::error!(err=%error, "NapCat acknowledgement mirror failed — reopen required");
                        cancel_acks.cancel();
                    }
                });

                // Outbound replies: the model emits an explicit target so we
                // never guess who to answer. Every send is recorded in the
                // durable operation ledger *before* it is written, and a
                // missing echo is recorded as unknown for reconciliation.
                // SPEC §35.6, §41.4 X03.
                let ledger = Arc::new(
                    OutboundLedger::with_store(Arc::clone(&store))
                        .with_account(napcat_config.bot_qq),
                );
                let mut outbox_rx = outbox_tx.subscribe();
                let reply_client = Arc::clone(&client);
                let cancel_replies = cancel_napcat.clone();
                tokio::spawn(async move {
                    loop {
                        let msg = tokio::select! {
                            msg = outbox_rx.recv() => msg,
                            _ = cancel_replies.cancelled() => break,
                        };
                        let msg = match msg {
                            Ok(msg) => msg,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(skipped=n, "outbox lagged — replies skipped");
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };

                        let Some((kind, target, text)) = parse_reply_directive(&msg.content) else {
                            continue;
                        };

                        let op = OutboundOperation::new(kind, target, text);
                        let op_id = op.operation_id;
                        if let Err(e) = ledger.record_intent(&op) {
                            tracing::warn!(err=%e, "outbound intent not durable — message not sent");
                            continue;
                        }

                        let send_result = match kind {
                            OutboundKind::SendGroup => {
                                reply_client
                                    .send_group_message_with_echo(target, &op.message, op.echo_token.clone())
                                    .await
                            }
                            OutboundKind::SendPrivate => {
                                reply_client
                                    .send_private_message_with_echo(target, &op.message, op.echo_token.clone())
                                    .await
                            }
                        };

                        match send_result {
                            Ok(receipt) => {
                                if let Err(e) = ledger.record_ack(op_id, receipt.retcode) {
                                    tracing::warn!(err=%e, op=%op_id, "failed to record outbound ack");
                                }
                                if receipt.is_success() {
                                    tracing::info!(op=%op_id, target, kind=kind.as_str(), "QQ reply sent");
                                } else {
                                    tracing::warn!(
                                        op=%op_id, status=%receipt.status, retcode=receipt.retcode,
                                        "NapCat rejected QQ reply"
                                    );
                                }
                            }
                            Err(e) => {
                                // The request may have been executed. Record the
                                // unknown effect and leave resolution to
                                // reconciliation; never blind-resend.
                                tracing::warn!(err=%e, op=%op_id, "QQ reply outcome unknown");
                                if let Err(e) = ledger.record_unknown(op_id) {
                                    tracing::warn!(err=%e, op=%op_id, "failed to record unknown outcome");
                                }
                            }
                        }
                    }
                });
                info!("NapCat connector started");
            }

            // 6. Start WebUI
            let ui = shenko_ui::server::UiServer::new(port)
                .with_inbox(subject_inbox.sender())
                .with_outbox(outbox_tx)
                .with_data_dir(data_dir.clone());
            println!("Shenko UI: http://0.0.0.0:{port}");
            println!("  Subject loop: running (continuous thought)");
            println!("  Inference: {}", if std::env::var("ANTHROPIC_API_KEY").is_ok() { "claude-opus-5-5" } else { "stub (no ANTHROPIC_API_KEY)" });

            // Run until Ctrl-C
            let _cancel_ui = cancel.clone();
            tokio::select! {
                res = ui.run() => {
                    if let Err(e) = res { tracing::error!("UI: {e}"); }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("shutdown signal");
                }
                _ = cancel.cancelled() => {
                    anyhow::bail!("runtime stopped after a connector failure; reopen persistent state for recovery");
                }
            }
            cancel.cancel();
            info!("Shenko stopped");
        }
    }

    Ok(())
}

fn napcat_inbox_event(
    event: shenko_channel_napcat::client::OneBotEvent,
) -> Result<shenko_subject::inbox::InboxEvent> {
    Ok(shenko_subject::inbox::InboxEvent {
        event_id: event.ingress_id.ok_or_else(|| anyhow::anyhow!("NapCat event missing durable ingress identity"))?,
        kind: shenko_subject::inbox::InboxEventKind::Message,
        payload: serde_json::to_value(&event)?,
        created_at: chrono::Utc::now(),
        source: "napcat".to_string(),
    })
}

/// Durably record a delegated child task as `Pending`. Returns the commit that
/// inserted it. The intent itself is sealed into the CAS, so the child's input
/// is re-readable later. SPEC §41.4 X02, §5.2.
///
/// `agent_id` is used as the conversation/owner placeholder, matching the
/// store's existing single-subject convention (`loop_runner` uses the same value
/// as `conversation_id` for inference requests).
fn persist_delegation(
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    task_id: shenko_types::ids::EntityId,
    intent: &shenko_types::subject::DelegationIntent,
    kind: &str,
) -> Result<shenko_types::commit::CommitRef> {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision, TaskRevision};
    use shenko_types::object::Digest;
    use shenko_types::store::WriteBatch;
    use shenko_types::task::{
        DispatchEligibility, ImplementationRef, TaskOwner, TaskRecord, TaskState,
    };

    let input_bytes = serde_json::to_vec(intent)?;
    let input_ref = store
        .cas
        .seal_object(&input_bytes, "application/json", "delegation", "json", 1)?
        .object;

    // Pre-commit placeholder, same convention as genesis: the ledger commit that
    // inserts this row is returned below and is the authoritative provenance.
    let pre_commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq(0));

    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id,
        conversation_id: agent_id,
        owner: TaskOwner::Conversation { id: agent_id },
        background: false,
        kind: kind.to_string(),
        definition_version: 1,
        // No seat implementation is bound yet. TODO(§35.4).
        implementation_ref: ImplementationRef {
            plugin_id: EntityId::nil(),
            artifact_digest: Digest::blake3(b""),
            registry_revision: 0,
        },
        input_ref,
        state: TaskState::Pending { checkpoint_ref: None },
        abort_request: None,
        memos: Default::default(),
        residual_case_ids: Vec::new(),
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: pre_commit,
        updated_commit: pre_commit,
    };

    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task);
    let receipt = store.commit(batch)?;
    Ok(receipt.commit_ref)
}

/// Parse an explicit QQ reply directive emitted by the subject:
/// `[reply:group:<group_id>] text` or `[reply:private:<user_id>] text`.
/// Continuation lines are included until the next directive.
fn parse_reply_directive(
    content: &str,
) -> Option<(shenko_channel_napcat::outbound::OutboundKind, u64, String)> {
    use shenko_channel_napcat::outbound::OutboundKind;

    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("[reply:") else { continue };
        let Some((head, tail)) = rest.split_once(']') else { continue };
        let Some((kind, target)) = head.split_once(':') else { continue };
        let Ok(target) = target.trim().parse::<u64>() else { continue };
        let kind = match kind.trim() {
            "group" => OutboundKind::SendGroup,
            "private" => OutboundKind::SendPrivate,
            _ => continue,
        };

        let mut body = String::from(tail.trim());
        for next in &lines[i + 1..] {
            if next.trim_start().starts_with("[reply:") { break; }
            if !body.is_empty() { body.push('\n'); }
            body.push_str(next.trim_end());
        }
        let body = body.trim().to_string();
        if body.is_empty() { continue; }
        return Some((kind, target, body));
    }
    None
}

/// Ensure genesis exists for this installation. Returns agent_id. SPEC §44.6
fn bootstrap_genesis(
    store: &shenko_store::store::Store,
    name: Option<&str>,
) -> Result<shenko_types::ids::EntityId> {
    use shenko_types::ids::*;
    use shenko_types::subject::*;
    use shenko_types::commit::CommitRef;
    use shenko_types::object::ObjectRef;

    // Check if genesis exists already
    let existing_agent_id = store.get_existing_agent_id()?;

    if let Some(agent_id) = existing_agent_id {
        info!(agent_id=%agent_id, "existing genesis found — resuming");
        return Ok(agent_id);
    }

    // First run — create genesis. SPEC §44.6
    let agent_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let genesis_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    // Seal the real seed artifacts into the CAS so the genesis object closure is
    // materialized and re-readable. SPEC §44.6.
    let seal_json = |value: &serde_json::Value| -> Result<ObjectRef> {
        let bytes = serde_json::to_vec(value)?;
        Ok(store
            .cas
            .seal_object(&bytes, "application/json", "seed", "json", 1)?
            .object)
    };

    let seed_manifest = seal_json(&serde_json::json!({
        "seed_version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "created": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "spec_version": "0.6.2",
    }))?;
    let initial_profile = seal_json(&serde_json::json!({
        "model": "claude-opus-5-5",
        "max_tokens": 4096,
        "source": "default",
    }))?;
    let home_dir = std::env::var("SHENKO_HOME")
        .map(|h| format!("{h}/home"))
        .unwrap_or_else(|_| "$SHENKO_HOME/home".to_string());
    let home_manifest = seal_json(&serde_json::json!({
        "home": home_dir,
        "wasm": true,
        "mounts": [],
    }))?;
    let bootstrap_event = seal_json(&serde_json::json!({
        "event": "bootstrap",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "kind": "genesis",
    }))?;

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id,
        launch_id,
        seed_manifest_ref: seed_manifest,
        initial_profile_ref: initial_profile,
        home_manifest_ref: home_manifest,
        bootstrap_event_ref: bootstrap_event,
        created_commit: CommitRef { ledger_id, seq: LedgerSeq(0) },
    };

    let first_step_id = EntityId::new();
    let head = SubjectBranchHead {
        agent_id,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id, genesis_id },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: CommitRef { ledger_id, seq: LedgerSeq(0) },
        next_step_id: first_step_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };

    let first_step = shenko_types::subject::SubjectStep {
        step_id: first_step_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: genesis.created_commit,
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        state: shenko_types::subject::SubjectStepState::Planned,
        input_plan_ref: None,
        context_revision: None,
        active_request_id: None,
        active_attempt_id: None,
        model_result_ref: None,
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };

    let receipt = store.initialize_genesis(&genesis, &head, &first_step)?;
    info!(agent_id=%agent_id, name=name.unwrap_or("(unnamed)"), commit=%receipt.commit_ref, "genesis created");

    Ok(agent_id)
}

#[cfg(test)]
mod recovery_integration_tests {
    use super::*;
    use shenko_channel_napcat::{NapCatClient, NapCatConfig, spool::IngressSpool};
    use shenko_types::{ids::*, subject::*};

    // Local I/L/CAS simulation: crash after S1, before I mirrors its ACK.
    // No provider, external account, model inference or production data is used.
    #[tokio::test]
    async fn reopen_reconciles_spool_from_committed_subject_disposition() {
        let dir = std::env::temp_dir().join(format!("shenko-continuity-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let spool_path = dir.join("ingress.sqlite");
        let spool = IngressSpool::open(&spool_path, 9).unwrap();
        let received = spool.ingest(serde_json::json!({
            "post_type":"message", "message_type":"group", "group_id":7,
            "self_id":9, "user_id":8, "message_id":42, "raw_message":"continue"
        })).unwrap().unwrap();
        drop(spool);

        let config = NapCatConfig { bot_qq: 9, ws_url: "not-a-url".into(), ..Default::default() };
        let (client, mut rx, actions) = NapCatClient::open_persistent(config.clone(), &spool_path).unwrap();
        assert!(NapCatClient::run_connection_loop(
            config, client.event_sender(), actions, client.pending_actions.clone(),
            client.spool.clone(), tokio_util::sync::CancellationToken::new(),
        ).await.is_err()); // Local URL rejection happens after durable replay.
        let inbox_event = napcat_inbox_event(rx.try_recv().unwrap()).unwrap();
        assert_eq!(inbox_event.event_id, received.spool_id);

        let store = shenko_store::store::Store::open(dir.join("control.sqlite"), dir.join("cas")).unwrap();
        let agent = bootstrap_genesis(&store, None).unwrap();
        let (head, mut step) = store.load_subject(agent).unwrap();
        assert!(store.event_disposition_commit(agent, inbox_event.event_id).unwrap().is_none());
        assert!(!client.spool.event(received.spool_id).unwrap().consumed);
        let object = store.cas.seal_object(b"continue", "text/plain", "test", "raw", 1).unwrap().object;
        step.input_plan_ref = Some(object.clone());
        step.state = SubjectStepState::Preparing;
        step.record_revision = RecordRevision(1);
        store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
        step.state = SubjectStepState::AwaitingModel;
        step.active_request_id = Some(EntityId::new());
        step.record_revision = RecordRevision(2);
        store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
        step.state = SubjectStepState::ResultRecorded;
        step.model_result_ref = Some(object.clone());
        step.record_revision = RecordRevision(3);
        store.upsert_step(&step, &head, RecordRevision(2)).unwrap();
        let next = EntityId::new();
        let decision = StepDecision {
            step_id: step.step_id, input_plan_ref: object.clone(), model_result_ref: Some(object),
            applied_steering_refs: vec![],
            event_dispositions: vec![EventDisposition { event_id: inbox_event.event_id, disposition: Disposition::Acknowledged }],
            delegated_task_intents: vec![], child_dialogue_intents: vec![], memory_intents: vec![],
            policy_intents: vec![], publication_intents: vec![], pending_obligations: vec![], evidence_refs: vec![], successor_step_id: next,
        };
        step.decision_ref = Some(store.cas.seal_object(
            &serde_json::to_vec(&decision).unwrap(), "application/json", "test", "StepDecision", 1,
        ).unwrap().object);
        step.state = SubjectStepState::Committed;
        step.record_revision = RecordRevision(4);
        let receipt = store.commit_step_and_advance_head(&step, &head, next).unwrap();
        // Intentionally omit ACK: model-side processing committed, I mirror is stale.
        assert!(!client.spool.event(received.spool_id).unwrap().consumed);
        drop(store);
        drop(client);

        let store = Arc::new(shenko_store::store::Store::open(dir.join("control.sqlite"), dir.join("cas")).unwrap());
        assert_eq!(bootstrap_genesis(&store, None).unwrap(), agent);
        let (dispatcher, _rx) = shenko_subject::dispatch::ChildDispatcher::new(agent);
        let restored = shenko_subject::loop_runner::SubjectLoopState::new(
            agent, Arc::new(shenko_subject::inbox::Inbox::new(8)), Arc::new(dispatcher), store.clone(),
        ).unwrap();
        assert_eq!(restored.head.branch_id, head.branch_id);
        assert_eq!(restored.head.committed_at, receipt.commit_ref);
        assert_eq!(restored.step.step_id, next);
        assert_eq!(restored.step.state, SubjectStepState::Planned);
        let spool = IngressSpool::open(&spool_path, 9).unwrap();
        let pending = spool.consume(8).unwrap();
        assert_eq!(pending.len(), 1);
        let commit = store.event_disposition_commit(agent, pending[0].spool_id).unwrap().unwrap();
        assert_eq!(commit, receipt.commit_ref);
        spool.ack(pending[0].spool_id, &commit.to_string()).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 0);
        assert_eq!(spool.event(received.spool_id).unwrap().raw, received.raw);
        assert!(spool.ingest(received.raw).unwrap().is_none());
        // Temporary evidence files are deliberately retained for inspection.
    }
}
