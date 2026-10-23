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

        Command::Start { port, name, napcat } => {
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
            ).with_store(Arc::clone(&store))
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
                    ..Default::default()
                };
                let (napcat_client, mut napcat_event_rx, napcat_action_rx) =
                    shenko_channel_napcat::client::NapCatClient::new(napcat_config.clone());
                let client = Arc::new(napcat_client);
                let spool = client.spool.clone();
                let event_tx = client.event_sender();
                let pending_actions = client.pending_actions.clone();
                let cancel_napcat = cancel.clone();

                // WebSocket reader/writer loop.
                tokio::spawn(shenko_channel_napcat::client::NapCatClient::run_connection_loop(
                    napcat_config.clone(),
                    event_tx,
                    napcat_action_rx,
                    pending_actions,
                    spool,
                    cancel_napcat.clone(),
                ));

                // Route NapCat events to the subject inbox; spool provides dedup
                // and gap recording.
                let inbox_tx = subject_inbox.sender();
                let cancel_events = cancel_napcat.clone();
                tokio::spawn(async move {
                    while let Some(ev) = napcat_event_rx.recv().await {
                        if cancel_events.is_cancelled() { break; }
                        let inbox_ev = shenko_subject::inbox::InboxEvent {
                            event_id: shenko_types::ids::EntityId::new(),
                            kind: shenko_subject::inbox::InboxEventKind::Message,
                            payload: serde_json::to_value(&ev).unwrap_or_default(),
                            created_at: chrono::Utc::now(),
                            source: "napcat".to_string(),
                        };
                        let _ = inbox_tx.send(inbox_ev).await;
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
            }
            cancel.cancel();
            info!("Shenko stopped");
        }
    }

    Ok(())
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
