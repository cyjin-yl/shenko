//! Shenko — personal agent framework entry point.
//! SPEC §44: seed/bootstrap startup with real subject loop + broker.
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use shenko_identity::IdentityService;
use shenko_types::identity::{IdentityChange, IdentityProfile, IdentitySource};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

mod node_sync;
mod runtime_owner;

#[derive(Parser, Debug)]
#[command(
    name = "shenko",
    about = "Shenko（深刻）— personal agent framework",
    version = "0.1.0"
)]
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
        /// Instance name given by the user. Omit it to let the agent name
        /// itself. Existing instances are edited with an explicit revision CAS.
        #[arg(long)]
        name: Option<String>,
        /// NapCat WebSocket URL (e.g. ws://127.0.0.1:3001)
        #[arg(long, env = "NAPCAT_WS_URL")]
        napcat: Option<String>,
        /// Account bound to the persistent ingress database (not an admin grant).
        #[arg(long, env = "NAPCAT_BOT_QQ", requires = "napcat")]
        napcat_account: Option<u64>,
    },
    /// Show or edit the instance's name, soul and personality — SPEC §6.
    /// Every edit is a new revision of the same agent; history is kept.
    Identity {
        #[arg(long)]
        set_name: Option<String>,
        #[arg(long)]
        set_soul: Option<String>,
        #[arg(long)]
        set_personality: Option<String>,
        /// Revision read by the editor; required for any mutation.
        #[arg(long)]
        expected_revision: Option<u64>,
        /// Print every revision, oldest first
        #[arg(long)]
        history: bool,
    },
    /// Handle a unified Node role/installation request from stdin.
    Node,
    /// Model-independent cognition and installation-package replication.
    Sync {
        #[command(subcommand)]
        command: node_sync::SyncCommand,
    },
    /// Inspect or request a subject-only boundary; this does not activate a candidate.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Validate configuration and exit
    Check,
    /// Print version info
    Version,
}

#[derive(Subcommand, Debug)]
enum RuntimeCommand {
    /// Ask the current subject to pause after a finite step.
    Pause {
        /// Stable ID for recovery after an uncertain write; generated if omitted.
        #[arg(long)]
        request_id: Option<shenko_types::ids::EntityId>,
    },
    /// Read the durable outcome of a previously issued request.
    Status { request_id: shenko_types::ids::EntityId },
    /// Cancel an unaccepted request or resume the same controller.
    Resume { request_id: shenko_types::ids::EntityId },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_writer(std::io::stderr)
        .with_target(true)
        .compact()
        .init();

    let data_dir = cli.data_dir.unwrap_or_else(|| {
        dirs_next::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("shenko")
    });

    // This guard outlives the Tokio runtime. In particular, cancellation of
    // an async task is not proof that a blocking/provider operation has ended.
    let (owner, data_dir) = if matches!(&cli.command, Command::Start { .. }) {
        let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: data_dir.clone(), ..Default::default()
        });
        // Reject an unrelated target before creating any owner or store file.
        bootstrap.validate_target()?;
        std::fs::create_dir_all(&data_dir)?;
        let data_dir = data_dir.canonicalize()?;
        let owner = runtime_owner::RuntimeOwner::try_acquire(&data_dir)
            .context("cannot establish exclusive ownership of this data directory")?;
        // Recheck after obtaining the guard: another starter may have finished
        // bootstrap between the initial read and lock acquisition.
        let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: data_dir.clone(), ..Default::default()
        });
        bootstrap.validate_target()?;
        owner.verify_path(&data_dir)?;
        (Some(owner), data_dir)
    } else {
        (None, data_dir)
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run(cli.command, data_dir));
    drop(runtime);
    drop(owner);
    result
}

async fn run(command: Command, data_dir: PathBuf) -> Result<()> {
    // Reuse the actual published C03 host: one native root, one install.
    let supervisor =
        shenko_kernel::supervisor::Supervisor::new(shenko_types::ids::RuntimeGeneration::default());
    let policy: Arc<dyn shenko_identity::IdentityPolicy> =
        Arc::new(shenko_identity::DefaultIdentityPolicy);
    let _identity_plugin = supervisor
        .spawn(shenko_identity::IdentityPlugin, policy)
        .await?;
    let identity_context = supervisor.context().clone();
    let identity_service = identity_context.try_service::<IdentityService>()?;
    let result: Result<()> = async {
    match command {
        Command::Node => node_sync::node_command(&data_dir)?,
        Command::Sync { command } => node_sync::run(command, data_dir.clone()).await?,
        Command::Runtime { command } => {
            let db_path = data_dir.join("shenko.db");
            anyhow::ensure!(db_path.is_file(), "no existing subject store in data directory");
            let store = shenko_store::store::Store::open(db_path, data_dir.join("cas"))?;
            let agent_id = store.get_existing_agent_id()?
                .ok_or_else(|| anyhow::anyhow!("no initialized subject"))?;
            let record = match command {
                RuntimeCommand::Pause { request_id } => {
                    let id = request_id.unwrap_or_default();
                    // Publish the caller's stable key even when commit outcome is
                    // uncertain; status reads it without replaying a request.
                    eprintln!("subject boundary request: {id}");
                    store.request_subject_quiesce(agent_id, id)?
                }
                RuntimeCommand::Status { request_id } => store.subject_quiesce(request_id)?
                    .ok_or_else(|| anyhow::anyhow!("quiesce request not found"))?,
                RuntimeCommand::Resume { request_id } => store.resume_subject_quiesce(agent_id, request_id)?,
            };
            println!("{}", serde_json::to_string_pretty(&record)?);
        }
        Command::Version => {
            let model = shenko_seed::ModelProfile::from_env();
            println!("Shenko（深刻）0.1.0  SPEC v0.6.2");
            println!("Model: {}", model.model.as_deref().unwrap_or("not_connected"));
            println!("Inference capacity: {:?}", model.capacity);
        }

        Command::Check => {
            info!(data_dir=%data_dir.display(), "check");
            let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
                data_dir: data_dir.clone(),
                ..Default::default()
            });
            bootstrap.validate_target()?;
            tokio::fs::create_dir_all(&data_dir).await?;
            let store = shenko_store::store::Store::open(
                data_dir.join("shenko.db"),
                data_dir.join("cas"),
            )?;
            println!(
                "Store: OK (SQLite {}, WAL={}, FTS5={})",
                store.capabilities.sqlite_version,
                store.capabilities.wal_mode,
                store.capabilities.fts5_available,
            );
            let prepared = bootstrap.prepare(store.ledger.ledger_id())?;
            println!("Home manifest: {}", prepared.manifest_path.display());
            println!("Home: {}", serde_json::to_string_pretty(&prepared.manifest)?);
            let subject = if store.get_existing_agent_id()?.is_some() {
                shenko_seed::BootstrapSubjectState::ExistingGenesis
            } else {
                shenko_seed::BootstrapSubjectState::PendingGenesis
            };
            let owner_grant = store.get_bootstrap_owner_grant(store.ledger.ledger_id())?;
            let view = bootstrap.view(
                store.ledger.ledger_id(),
                subject,
                shenko_seed::ModelProfile::from_env(),
                owner_grant,
            );
            println!("Bootstrap: {}", serde_json::to_string_pretty(&view)?);
            drop(prepared);
            println!("OK");
        }

        Command::Identity {
            set_name,
            set_soul,
            set_personality,
            expected_revision,
            history,
        } => {
            tokio::fs::create_dir_all(&data_dir).await?;
            let store =
                shenko_store::store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
            let agent_id = store
                .get_existing_agent_id()?
                .ok_or_else(|| anyhow::anyhow!("no instance yet; run `shenko start` first"))?;
            let mut current = store
                .latest_identity(agent_id)?
                .ok_or_else(|| anyhow::anyhow!("identity not initialized"))?;
            let changes = [
                set_name.map(IdentityChange::Name),
                set_soul.map(IdentityChange::Soul),
                set_personality.map(IdentityChange::Personality),
            ];
            let changes: Vec<_> = changes.into_iter().flatten().collect();
            if !changes.is_empty() {
                let expected = expected_revision.ok_or_else(|| {
                    anyhow::anyhow!(
                        "editing requires --expected-revision from the profile you read"
                    )
                })?;
                anyhow::ensure!(
                    current.revision == expected,
                    "IDENTITY_REVISION_CONFLICT: expected {expected}, actual {}",
                    current.revision
                );
                if let Some(next) =
                    identity_service.revise(&current, &changes, IdentitySource::Operator)
                {
                    store.append_identity(&next, expected)?;
                    current = next;
                }
            }
            let shown = if history {
                store.identity_history(agent_id)?
            } else {
                vec![current]
            };
            for profile in shown {
                println!("{}", serde_json::to_string_pretty(&profile)?);
            }
        }

        Command::Start {
            port,
            name,
            napcat,
            napcat_account,
        } => {
            if napcat.is_some() {
                anyhow::ensure!(
                    napcat_account.is_some_and(|account| account > 0),
                    "NapCat persistent ingress requires --napcat-account or NAPCAT_BOT_QQ"
                );
            }
            info!(
                data_dir=%data_dir.display(), port,
                name=name.as_deref().unwrap_or("(unnamed)"),
                "Shenko starting"
            );

            let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
                data_dir: data_dir.clone(),
                agent_name: name.clone(),
                napcat_ws_url: napcat.clone(),
            });
            bootstrap.validate_target()?;
            let model_profile = shenko_seed::ModelProfile::from_env();

            // 1. Open persistent store and prepare the actual WASI home.
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

            let prepared_home = bootstrap.prepare(store.ledger.ledger_id())?;
            info!(manifest=%prepared_home.manifest_path.display(), "WASM home ready");

            // 2. Bootstrap genesis if first run — SPEC §44.6. The genesis
            // closure references the actual home manifest and guest artifact.
            let agent_id = bootstrap_genesis(
                &store,
                name.as_deref(),
                &prepared_home.manifest,
                &model_profile,
            )?;
            drop(prepared_home);
            let identity = adopt_identity(&store, agent_id, name.as_deref(), &identity_service)?;
            info!(
                revision = identity.revision,
                name = identity.display_name.as_deref().unwrap_or("(unnamed)"),
                naming = ?identity.naming,
                "identity ready"
            );

            drop(node_sync::open_node(&data_dir, &store)?);
            let runtime_instance = shenko_types::ids::EntityId::new();
            // Deterministic recovery services remain available without an LLM.
            let cancel = tokio_util::sync::CancellationToken::new();
            let subject_inbox = Arc::new(shenko_subject::inbox::Inbox::new(1024));
            // Establish recovery consumers BEFORE ResultRecorded can take fast S1.
            store.start_subject_dispatcher()?;
            store.settle_subject_memory()?;
            store.reconcile_subject_publications()?;
            // Native finite children use the same Cordis root and persistent
            // ledger. The handler is deliberately narrow; generic model goals
            // remain parked instead of becoming a fictitious completed LLM run.
            let _native_child_plugin = supervisor
                .spawn(
                    shenko_subject::NativeChildPlugin,
                    (
                        Arc::clone(&store),
                        shenko_types::ids::RuntimeGeneration::default(),
                    ),
                )
                .await?;
            let printf_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::HostPrintfHandler::new(Arc::clone(&store)));
            let _native_printf_plugin = supervisor
                .spawn(shenko_subject::NativeTaskHandlerPlugin, printf_handler)
                .await?;
            let history_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::HistoryHandler::new(Arc::clone(&store)));
            let _native_history_plugin = supervisor
                .spawn(shenko_subject::NativeTaskHandlerPlugin, history_handler)
                .await?;
            // Built-in, replaceable self-explanation — SPEC §38, R124. The
            // provider is seeded from compiled-in material now; the real
            // `self://runtime/capabilities` snapshot is installed below once
            // the native and awareness registries are populated.
            let self_knowledge = Arc::new(shenko_awareness_core::SelfKnowledgeProvider::new(
                option_env!("SHENKO_SOURCE_REVISION").unwrap_or("unbound"),
                "0.6.2",
            ));
            let self_knowledge_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::SelfKnowledgeHandler::new(
                    Arc::clone(&store),
                    Arc::clone(&self_knowledge),
                ));
            let _native_self_knowledge_plugin = supervisor
                .spawn(
                    shenko_subject::NativeTaskHandlerPlugin,
                    self_knowledge_handler,
                )
                .await?;
            let native_child = supervisor
                .context()
                .try_service::<shenko_subject::NativeChildService>()?;
            native_child.recover()?;
            // Awareness is installed capability, not kernel business logic:
            // the shipped seeds plus any packages under data_dir/awareness
            // become advisory perspectives for the model. Nothing here
            // dispatches on keywords; every enabled unit travels as bounded
            // advisory text.
            let awareness = Arc::new(shenko_awareness_core::AwarenessRegistry::with_seeds());
            let awareness_packages = data_dir.join("awareness");
            if awareness_packages.is_dir() {
                for unit in awareness.install_from_dir(&awareness_packages)? {
                    info!(unit = %unit, "awareness unit installed");
                }
            }
            // Record what actually registered so self-knowledge answers from
            // the real registries instead of a stale product feature list.
            self_knowledge.install_runtime_capabilities(
                &native_child.installed_definitions(),
                &awareness.list(),
            );
            let consumer_cancel = tokio_util::sync::CancellationToken::new();
            let dispatch_store = Arc::clone(&store);
            let dispatch_native = Arc::clone(&native_child);
            let dispatch_results = model_profile.configured.then(|| subject_inbox.sender());
            let dispatch_stop = consumer_cancel.clone();
            let failure_cancel = cancel.clone();
            let dispatcher_task = tokio::spawn(async move {
                let result = shenko_subject::dispatch::run_committed_dispatcher(
                    dispatch_store,
                    dispatch_native,
                    agent_id,
                    dispatch_results,
                    dispatch_stop,
                ).await;
                if result.is_err() { failure_cancel.cancel(); }
                result
            });
            let _consumer_stop_on_exit = consumer_cancel.clone().drop_guard();
            let replication_task = node_sync::spawn_runtime(
                Arc::clone(&store), data_dir.clone(), consumer_cancel.clone(),
            );
            if !model_profile.configured {
                node_sync::observe(&data_dir, &store, runtime_instance, false)?;
                if napcat.is_some() {
                    tracing::warn!("NapCat not started without a subject model; durable bootstrap remains available");
                }
                let owner_grant =
                    store.get_bootstrap_owner_grant(store.ledger.ledger_id())?;
                let view = bootstrap.view(
                    store.ledger.ledger_id(),
                    shenko_seed::BootstrapSubjectState::ExistingGenesis,
                    model_profile.clone(),
                    owner_grant,
                );
                println!("Bootstrap: {}", serde_json::to_string_pretty(&view)?);
                let runtime_result =
                    run_deterministic_ui(port, store, data_dir, model_profile, cancel).await;
                consumer_cancel.cancel();
                match tokio::time::timeout(std::time::Duration::from_secs(2), dispatcher_task).await {
                    Ok(result) => result??,
                    Err(_) => tracing::warn!("dispatcher stop pending; durable work retained"),
                }
                replication_task.await?;
                return runtime_result;
            }

            // Start inference only after the model-independent recovery services.
            let (context_head, _) = store.load_subject(agent_id)?;
            let context_registry = Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
                shenko_context_policy::WindowLedger::new(Arc::new(
                    shenko_context_policy::StoreWindowLog::new(
                        Arc::clone(&store), shenko_types::ids::EntityId(context_head.branch_id.0),
                    ),
                )),
            ));
            context_registry.set_preference(vec![
                shenko_compact_snap::POLICY_ID.into(), shenko_compact_budget::POLICY_ID.into(),
            ]);
            let _context_host = supervisor
                .spawn(shenko_context_policy::ContextPolicyHost, Arc::clone(&context_registry))
                .await?;
            let _budget_policy = supervisor
                .spawn(shenko_compact_budget::BudgetCompactPlugin, shenko_compact_budget::BudgetConfig::default())
                .await?;
            let _snap_policy = supervisor
                .spawn(shenko_compact_snap::SnapCompactPlugin, shenko_compact_snap::SnapConfig::default())
                .await?;
            let (broker, broker_handle) = shenko_broker::broker::Broker::new()?;
            let capabilities = match broker.discover_capabilities().await {
                Ok(capabilities) => Some(capabilities),
                Err(error) => {
                    tracing::warn!(error=%error, "model metadata unavailable; automatic context compaction disabled");
                    None
                }
            };
            tokio::spawn(broker.run());
            let hook_registry = Arc::new(shenko_kernel::hooks::HookRegistry::new());
            // Outbound path: subject model output → UI timeline. SPEC §5.1, §21.12
            let (outbox_tx, _outbox_rx) =
                tokio::sync::broadcast::channel::<shenko_subject::outbox::OutboxMessage>(64);
            let loop_state = shenko_subject::loop_runner::SubjectLoopState::new(
                agent_id,
                Arc::clone(&subject_inbox),
                Arc::clone(&store),
                identity_context.clone(),
            )?
            .with_broker(broker_handle)
            .with_model_capabilities(capabilities.clone())
            .with_native_capabilities(native_child.installed_prompt_sections())
            .with_awareness(awareness)
            .with_hooks(Arc::clone(&hook_registry))
            .with_outbox(outbox_tx.clone());

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
                let cancel_napcat = consumer_cancel.clone();

                // A storage failure closes the connector and stops this runtime;
                // silently leaving an apparently healthy UI would hide lost reception.
                let connection_spool = Arc::clone(&spool);
                let connection_config = napcat_config.clone();
                let connection_cancel = cancel_napcat.clone();
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
                let reply_client = Arc::clone(&client);
                let reply_store = Arc::clone(&store);
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
                        // Ready index, not the broadcast buffer, owns the work.
                        for work in reply_store.committed_intents(Some("publication"), Some("ready"), 32)? {
                            let publication: shenko_types::subject::PublicationIntent = serde_json::from_value(work.payload)?;

                            let (kind, target) = match publication.target {
                                shenko_types::output::PublicationTarget::QqGroup(id) => {
                                    (OutboundKind::SendGroup, id)
                                }
                                shenko_types::output::PublicationTarget::QqPrivate(id) => {
                                    (OutboundKind::SendPrivate, id)
                                }
                            };
                            let mut op =
                                OutboundOperation::new(kind, target, publication.content.clone());
                            op.operation_id = publication.intent_id;
                            op.echo_token = publication.intent_id.to_string();
                            let op_id = op.operation_id;
                            if !ledger.record_intent(&op)? { continue; }

                            let send_result = match kind {
                                OutboundKind::SendGroup => {
                                    reply_client
                                        .send_group_message_with_echo(
                                            target,
                                            &op.message,
                                            op.echo_token.clone(),
                                        )
                                        .await
                                }
                                OutboundKind::SendPrivate => {
                                    reply_client
                                        .send_private_message_with_echo(
                                            target,
                                            &op.message,
                                            op.echo_token.clone(),
                                        )
                                        .await
                                }
                            };

                            match send_result {
                                Ok(receipt) => {
                                    ledger.record_response(op_id, &receipt)?;
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
                                    ledger.record_unknown(op_id, &e.to_string())?;
                                }
                            }
                        }
                    }
                    Ok(())
                    }.await;
                    if let Err(e)=result { tracing::error!(err=%e,"publication consumer stopped; reopen writer and reconcile"); reply_failure.cancel(); }
                });
                info!("NapCat connector started");
            }

            // Connector/ACK storage failures also stop admission in the subject.
            let connector_failure = cancel.clone();
            let connector_stop = consumer_cancel.clone();
            tokio::spawn(async move { connector_stop.cancelled().await; connector_failure.cancel(); });
            let cancel_subject = cancel.clone();
            let mut subject_task = tokio::spawn(shenko_subject::loop_runner::run_subject_loop(
                loop_state,
                cancel_subject,
            ));
            node_sync::observe(&data_dir, &store, runtime_instance, true)?;
            info!("subject loop started");


            // 6. Start WebUI
            let ui = shenko_ui::server::UiServer::new(port)
                .with_inbox(subject_inbox.sender())
                .with_outbox(outbox_tx)
                .with_store(Arc::clone(&store))
                .with_context(Arc::clone(&context_registry), capabilities)
                .with_data_dir(data_dir.clone())
                .with_inference_status(model_profile.model.clone(), model_profile.configured);
            println!("Shenko UI: http://0.0.0.0:{port}");
            println!("  Subject task: supervised (durable boundary pause may be active)");
            println!(
                "  Inference: {} (configured; probe/capability evidence separate)",
                model_profile.model.as_deref().unwrap_or("not_connected")
            );

            // Supervise the actual subject task, including panics. Stop claiming
            // availability before waiting for the current durable boundary.
            let live = ui.ws_state().live;
            let mut subject_finished = false;
            let runtime_result: Result<()> = tokio::select! {
                result = &mut subject_task => {
                    subject_finished = true;
                    match result {
                        Ok(Err(error)) => Err(error.into()),
                        Err(error) => Err(error.into()),
                        Ok(Ok(())) => Err(anyhow::anyhow!("subject stopped unexpectedly")),
                    }
                }
                result = ui.run() => result,
                result = tokio::signal::ctrl_c() => {
                    result?;
                    info!("shutdown signal; settling subject at a recoverable boundary");
                    Ok(())
                }
                _ = cancel.cancelled() => Err(anyhow::anyhow!("runtime stopped after connector failure; reopen for recovery")),
            };
            live.set_wired(false);
            subject_inbox.close().await;
            cancel.cancel();
            if !subject_finished {
                // Do not drop an admitted request merely because Ctrl-C arrived.
                // A timeout is an observation limit, never proof the provider stopped.
                match tokio::time::timeout(std::time::Duration::from_secs(15), &mut subject_task)
                    .await
                {
                    Ok(Ok(result)) => result?,
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => {
                        consumer_cancel.cancel();
                        let (_, step) = store.load_subject(agent_id)?;
                        anyhow::bail!(
                            "shutdown settlement timed out: durable step {} is {:?}, request {:?}; remote outcome is NOT proven stopped, reopen/reconcile before dispatch",
                            step.step_id,
                            step.state,
                            step.active_request_id
                        );
                    }
                }
            }
            // S1 has settled or left a durable unresolved step. Do not drop the
            // consumers first. Their unfinished work remains queryable on reopen.
            consumer_cancel.cancel();
            match tokio::time::timeout(std::time::Duration::from_secs(2), dispatcher_task).await {
                Ok(result) => result??,
                Err(_) => tracing::warn!("dispatcher stop pending; durable work retained"),
            }
            replication_task.await?;
            runtime_result?;
            info!("Shenko stopped");
        }
    }

    Ok(())
    }.await;
    supervisor.stop().await?;
    result
}

/// Deterministic no-model service: keep the existing UI, configuration and
/// health endpoints available without constructing a broker or a fake thought
/// loop. No inbox/outbox is wired, so browsers cannot mistake it for thinking.
async fn run_deterministic_ui(
    port: u16,
    store: Arc<shenko_store::store::Store>,
    data_dir: PathBuf,
    model_profile: shenko_seed::ModelProfile,
    failure: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let ui = shenko_ui::server::UiServer::new(port)
        .with_store(store)
        .with_data_dir(data_dir)
        .with_inference_status(model_profile.model, model_profile.configured);
    println!("Shenko UI: http://0.0.0.0:{port}");
    println!("  Subject loop: not started (model_binding needed)");
    println!("  Inference: not_connected; capacity blocked");
    let result = tokio::select! {
        result = ui.run() => result,
        _ = failure.cancelled() => Err(anyhow::anyhow!("deterministic recovery stopped; reopen for recovery")),
        result = tokio::signal::ctrl_c() => {
            result?;
            info!("shutdown signal; deterministic bootstrap stopped without model effects");
            Ok(())
        }
    };
    result?;
    info!("Shenko stopped");
    Ok(())
}

fn napcat_inbox_event(
    event: shenko_channel_napcat::client::OneBotEvent,
) -> Result<shenko_subject::inbox::InboxEvent> {
    Ok(shenko_subject::inbox::InboxEvent {
        event_id: event
            .ingress_id
            .ok_or_else(|| anyhow::anyhow!("NapCat event missing durable ingress identity"))?,
        kind: shenko_subject::inbox::InboxEventKind::Message,
        payload: serde_json::to_value(&event)?,
        created_at: chrono::Utc::now(),
        source: "napcat".to_string(),
    })
}

/// Make sure the agent has an identity revision, and apply a user-given
/// `--name` only for initialization. Existing edits require an explicit CAS. SPEC §6, R117.
fn adopt_identity(
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    name: Option<&str>,
    identity: &IdentityService,
) -> Result<IdentityProfile> {
    let genesis = identity.genesis(agent_id, name);
    let current = store.initialize_identity(&genesis)?;
    anyhow::ensure!(
        name.is_none() || current.display_name == genesis.display_name,
        "IDENTITY_INITIAL_NAME_CONFLICT: start --name only initializes; use identity --expected-revision {} to edit",
        current.revision
    );
    Ok(current)
}

/// Ensure genesis exists for this installation. Returns agent_id. SPEC §44.6.
/// New geneses bind the real prepared home. Existing geneses are immutable and
/// are never replaced merely to upgrade their manifest representation.
fn bootstrap_genesis(
    store: &shenko_store::store::Store,
    name: Option<&str>,
    home: &shenko_seed::HomeManifest,
    model_profile: &shenko_seed::ModelProfile,
) -> Result<shenko_types::ids::EntityId> {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::object::ObjectRef;
    use shenko_types::subject::*;

    if let Some(agent_id) = store.get_existing_agent_id()? {
        let genesis = store
            .get_subject_genesis(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("existing subject has no genesis"))?;
        let read_json = |object: &ObjectRef, label: &str| -> Result<serde_json::Value> {
            let path = store.cas.verified_object_path(object)?;
            let bytes = std::fs::read(&path)?;
            serde_json::from_slice(&bytes).map_err(|error| {
                anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: decode {label}: {error}")
            })
        };
        let seed = read_json(&genesis.seed_manifest_ref, "seed manifest")?;
        read_json(&genesis.initial_profile_ref, "initial model profile")?;
        read_json(&genesis.bootstrap_event_ref, "bootstrap event")?;
        if let Some(value) = seed.get("home_manifest_ref") {
            let nested: ObjectRef = serde_json::from_value(value.clone()).map_err(|error| {
                anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: seed home reference: {error}")
            })?;
            anyhow::ensure!(
                nested.digest == genesis.home_manifest_ref.digest,
                "SEED_CLOSURE_INCOMPLETE: seed/genesis home references differ; data preserved"
            );
        }
        if let Some(value) = seed.get("home_probe_ref") {
            let probe: ObjectRef = serde_json::from_value(value.clone()).map_err(|error| {
                anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: seed guest reference: {error}")
            })?;
            store.cas.verified_object_path(&probe)?;
        }

        let home_path = store.cas.verified_object_path(&genesis.home_manifest_ref)?;
        let bytes = std::fs::read(home_path)?;
        match serde_json::from_slice::<shenko_seed::HomeManifest>(&bytes) {
            Ok(stored) => {
                if stored != *home {
                    let origin = store.cognition_subject_origin(agent_id)?;
                    anyhow::ensure!(origin.as_ref().is_some_and(|origin|
                        origin.genesis_id == genesis.genesis_id
                        && origin.source_ledger_id == genesis.created_commit.ledger_id),
                        "DATA_PATH_CONFLICT: genesis and current home manifests differ without verified replication provenance; data preserved");
                    anyhow::ensure!(stored.schema_version == home.schema_version
                        && stored.runtime == home.runtime && stored.wasi == home.wasi
                        && stored.checkpoint == home.checkpoint,
                        "WASM_PROFILE_UNAVAILABLE: replicated seed contract differs from this node's verified local home; no runtime success claimed");
                    info!(agent_id=%agent_id,
                        "replicated immutable genesis retained; using this node's independently prepared local home, not foreign mount paths");
                }
            }
            Err(error) => {
                // Before C05 the genesis contained only a descriptive placeholder
                // and no production runtime used it. Recognize exactly that shape;
                // arbitrary corrupt closures still stop with their data intact.
                let legacy: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
                    anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: decode genesis home: {error}")
                })?;
                let legacy_placeholder = legacy.get("wasm").and_then(|v| v.as_bool()) == Some(true)
                    && legacy.get("home").and_then(|v| v.as_str()).is_some()
                    && legacy
                        .get("mounts")
                        .and_then(|v| v.as_array())
                        .is_some_and(|mounts| mounts.is_empty());
                anyhow::ensure!(
                    legacy_placeholder,
                    "SEED_CLOSURE_INCOMPLETE: unrecognized genesis home manifest; data preserved"
                );
                tracing::warn!(
                    agent_id=%agent_id,
                    "legacy genesis home placeholder retained; current prepared home is not a new subject"
                );
            }
        }
        info!(agent_id=%agent_id, "existing genesis found — resuming");
        return Ok(agent_id);
    }

    let agent_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let genesis_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    let seal_json = |value: &serde_json::Value| -> Result<ObjectRef> {
        let bytes = serde_json::to_vec(value)?;
        Ok(store
            .cas
            .seal_object(&bytes, "application/json", "seed", "json", 1)?
            .object)
    };

    // The home object and actual WASI probe are sealed before a manifest may
    // reference them. This is a bootstrap-home closure, not a claim that every
    // product plugin or VM live state has been captured.
    let home_manifest = seal_json(&serde_json::to_value(home)?)?;
    let home_probe = store
        .cas
        .seal_object(
            shenko_seed::HOME_PROBE_WAT.as_bytes(),
            "application/wat",
            "seed",
            "home_probe",
            1,
        )?
        .object;
    let seed_manifest = seal_json(&serde_json::json!({
        "seed_version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "created": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "spec_version": "0.6.2",
        "runtime": home.runtime,
        "runtime_generation": home.runtime_generation,
        "home_manifest_ref": home_manifest.clone(),
        "home_probe_ref": home_probe,
        "closure_level": "bootstrap_home_only",
        "complete_product_seed": false,
    }))?;
    let initial_profile = seal_json(&serde_json::to_value(model_profile)?)?;
    let bootstrap_event = seal_json(&serde_json::json!({
        "event": "bootstrap",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "kind": "genesis",
        "installation_id": home.installation_id,
        "seed_manifest_ref": seed_manifest.clone(),
        "home_manifest_ref": home_manifest.clone(),
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
        created_commit: CommitRef {
            ledger_id,
            seq: LedgerSeq(0),
        },
    };

    let first_step_id = EntityId::new();
    let head = SubjectBranchHead {
        agent_id,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: CommitRef {
            ledger_id,
            seq: LedgerSeq(0),
        },
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
        model_outcome_ref: None,
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
    use shenko_types::{
        ids::*,
        operation::{EffectStatus, OperationOutcome, OperationState},
        subject::*,
        task::{AbortScope, TaskOutcome, TaskState},
    };

    // Local I/L/CAS simulation: crash after S1, before I mirrors its ACK.
    // No provider, external account, model inference or production data is used.
    #[tokio::test]
    async fn reopen_reconciles_spool_from_committed_subject_disposition() {
        let dir = std::env::temp_dir().join(format!("shenko-continuity-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let spool_path = dir.join("ingress.sqlite");
        let spool = IngressSpool::open(&spool_path, 9).unwrap();
        let received = spool
            .ingest(serde_json::json!({
                "post_type":"message", "message_type":"group", "group_id":7,
                "self_id":9, "user_id":8, "message_id":42, "raw_message":"continue"
            }))
            .unwrap()
            .unwrap();
        drop(spool);

        let config = NapCatConfig {
            bot_qq: 9,
            ws_url: "ws://127.0.0.1:0".into(),
            ..Default::default()
        };
        let (client, mut rx, actions) =
            NapCatClient::open_persistent(config.clone(), &spool_path).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let connector = tokio::spawn(NapCatClient::run_connection_loop(
            config,
            client.event_sender(),
            actions,
            client.pending_actions.clone(),
            client.spool.clone(),
            cancel.clone(),
        ));
        // Delivery now runs independently of connection establishment.
        let replayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let inbox_event = napcat_inbox_event(replayed).unwrap();
        cancel.cancel();
        connector.await.unwrap().unwrap();
        assert_eq!(inbox_event.event_id, received.spool_id);

        let store = Arc::new(shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        let model_profile = shenko_seed::ModelProfile::from_env();
        let agent = bootstrap_genesis(
            &store,
            None,
            &prepared_home.manifest,
            &model_profile,
        )
        .unwrap();
        let (head, mut step) = store.load_subject(agent).unwrap();
        assert!(
            store
                .event_disposition_commit(agent, inbox_event.event_id)
                .unwrap()
                .is_none()
        );
        assert!(!client.spool.event(received.spool_id).unwrap().consumed);
        let object = store
            .cas
            .seal_object(b"continue", "text/plain", "test", "raw", 1)
            .unwrap()
            .object;
        store.stage_subject_inputs(&head, &step, &[shenko_store::SubjectHistoryInput {
            event_id: inbox_event.event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1,"text":inbox_event.payload.to_string(),"inbox_event":&inbox_event}),
        }]).unwrap();
        step.input_plan_ref = Some(prepare_fixture_s0(&store, &head, &step));
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
        step.model_outcome_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                        schema_version: 2,
                        request_id: step.active_request_id.unwrap(),
                        result_ref: step.model_result_ref.clone().unwrap(),
                        stop_reason: "end_turn".into(),
                        raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                        usage: None,
                    })
                    .unwrap(),
                    "application/json",
                    "model_outcomes",
                    "SubjectModelOutcome",
                    2,
                )
                .unwrap()
                .object,
        );
        store.upsert_step(&step, &head, RecordRevision(2)).unwrap();
        let next = EntityId::new();
        let identity_context = cordis_core::Context::new();
        let _publication = identity_context
            .provide(Arc::new(IdentityService::new(Arc::new(
                shenko_identity::DefaultIdentityPolicy,
            ))))
            .unwrap();
        let identity_service = identity_context.try_service::<IdentityService>().unwrap();
        let _context = identity_context.provide(Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
            shenko_context_policy::WindowLedger::new(Arc::new(
                shenko_context_policy::StoreWindowLog::new(store.clone(), EntityId(head.branch_id.0)),
            )),
        ))).unwrap();
        let decision = StepDecision {
            schema_version: 6,
            output: shenko_subject::output::parse_output(step.step_id, object.clone(), "continue", &identity_service, None)
            .unwrap(),
            identity_intent: None,
            step_id: step.step_id,
            input_plan_ref: step.input_plan_ref.clone().unwrap(),
            model_result_ref: Some(object),
            applied_steering_refs: vec![],
            event_dispositions: vec![EventDisposition {
                event_id: inbox_event.event_id,
                disposition: Disposition::Acknowledged,
            }],
            delegated_task_intents: vec![],
            child_dialogue_intents: vec![],
            memory_intents: vec![],
            policy_intents: vec![],
            publication_intents: vec![],
            pending_obligations: vec![],
            evidence_refs: vec![],
            successor_step_id: next,
        };
        step.decision_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&decision).unwrap(),
                    "application/json",
                    "test",
                    "StepDecision",
                    6,
                )
                .unwrap()
                .object,
        );
        step.state = SubjectStepState::Committed;
        step.record_revision = RecordRevision(4);
        let receipt = store
            .commit_step_and_advance_head(&step, &head, next)
            .unwrap();
        // Intentionally omit ACK: model-side processing committed, I mirror is stale.
        assert!(!client.spool.event(received.spool_id).unwrap().consumed);
        drop(store);
        drop(client);

        let store = Arc::new(
            shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap(),
        );
        assert_eq!(
            bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap(),
            agent
        );
        let restored = shenko_subject::loop_runner::SubjectLoopState::new(
            agent,
            Arc::new(shenko_subject::inbox::Inbox::new(8)),
            store.clone(),
            identity_context,
        )
        .unwrap();
        assert_eq!(restored.head.branch_id, head.branch_id);
        assert_eq!(restored.head.committed_at, receipt.commit_ref);
        assert_eq!(restored.step.step_id, next);
        assert_eq!(restored.step.state, SubjectStepState::Planned);
        let spool = IngressSpool::open(&spool_path, 9).unwrap();
        let pending = spool.consume(8).unwrap();
        assert_eq!(pending.len(), 1);
        let commit = store
            .event_disposition_commit(agent, pending[0].spool_id)
            .unwrap()
            .unwrap();
        assert_eq!(commit, receipt.commit_ref);
        spool.ack(pending[0].spool_id, &commit.to_string()).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 0);
        assert_eq!(spool.event(received.spool_id).unwrap().raw, received.raw);
        assert!(spool.ingest(received.raw).unwrap().is_none());
        // Temporary evidence files are deliberately retained for inspection.
    }

    fn open_native_fixture(dir: &std::path::Path) -> shenko_store::store::Store {
        shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap()
    }

    fn prepare_fixture_s0(
        store: &Arc<shenko_store::store::Store>,
        head: &shenko_types::subject::SubjectBranchHead,
        step: &shenko_types::subject::SubjectStep,
    ) -> shenko_types::object::ObjectRef {
        let scope = EntityId(head.branch_id.0);
        let staged = store.stage_subject_inputs(head, step, &[]).unwrap();
        let input_raw_event_ids = staged.iter().map(|row| row.event_id).collect::<Vec<_>>();
        let (events, messages): (Vec<_>, Vec<_>) = staged.iter().map(|row| {
            let data: serde_json::Value = serde_json::from_str(&row.data).unwrap();
            let text = data.get("text").and_then(serde_json::Value::as_str).unwrap_or(&row.data);
            (data.get("inbox_event").cloned().unwrap(),
                shenko_broker::request::InferenceMessage::text("user", text))
        }).unzip();
        let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(store.clone(), scope),
        ));
        let profile = shenko_types::object::Digest::blake3(b"native-recovery-fixture");
        ledger.rebind_model(1, &profile.0).unwrap();
        let window = ledger.current().unwrap();
        let leaf_seq = store.session_leaf_seq(scope).unwrap()
            .map(|seq| u64::try_from(seq).unwrap());
        let input = serde_json::json!({
            "schema_version":10, "step_id":step.step_id, "step_no":step.step_no,
            "events":events, "provider_binding":null,
            "payload":shenko_broker::request::InferencePayload {
                system_prompt:"native recovery fixture".into(), messages,
                max_tokens:Some(4096), tools:vec![], native_options:Default::default(),
            },
            "semantic_wait":store.semantic_wait(head.agent_id).unwrap(), "identity":store.latest_identity(head.agent_id).unwrap(),
            "identity_provider_revision":"shenko.identity/default@1",
            "context":{
                "source":shenko_store::SubjectContextRef {
                    scope_id:scope, source_commit_seq:store.current_commit_seq().unwrap(), leaf_seq,
                    input_raw_event_ids, window_id:window.id,
                    window_generation:window.generation, model_generation:1 },
                "profile_digest":profile,
                "estimated_input_tokens":null, "budget_input_tokens":null,
                "trigger_budget_input_tokens":null,
                "budget_provenance":"native fixture without measured current input",
                "reserved_output_tokens":4096, "unfinished_call_ids":[],
                "capabilities":null, "observed_input_tokens":null,
                "token_counter":"heuristic:cjk-1-other-4@1; not a provider tokenizer",
                "policy_contract":shenko_context_policy::CONTRACT_VERSION,
                "selection":{"outcome":"unavailable","reason":"fixture_without_model_capabilities"},
            },
        });
        store.cas.seal_object(&serde_json::to_vec(&input).unwrap(), "application/json",
            "input_plans", "subject_step_input", 10).unwrap().object
    }

    /// Deterministic model-output fixture admitted through the real parser and S1
    /// transaction. No model provider is invoked by this test helper.
    fn commit_native_fixture_step(
        store: &Arc<shenko_store::store::Store>,
        model_text: &str,
        task_ids: &[EntityId],
        event_dispositions: Vec<EventDisposition>,
    ) -> shenko_types::commit::CommitRef {
        let (head, mut step) = store
            .load_subject(store.get_existing_agent_id().unwrap().unwrap())
            .unwrap();
        let object = store
            .cas
            .seal_object(model_text.as_bytes(), "text/plain", "test", "raw", 1)
            .unwrap()
            .object;
        step.input_plan_ref = Some(prepare_fixture_s0(store, &head, &step));
        step.state = SubjectStepState::Preparing;
        step.record_revision = RecordRevision(1);
        store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
        step.state = SubjectStepState::AwaitingModel;
        step.active_request_id = Some(EntityId::new());
        step.record_revision = RecordRevision(2);
        store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
        step.state = SubjectStepState::ResultRecorded;
        step.model_result_ref = Some(object.clone());
        step.model_outcome_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&SubjectModelOutcome {
                        schema_version: 2,
                        request_id: step.active_request_id.unwrap(),
                        result_ref: object.clone(),
                        stop_reason: "end_turn".into(),
                        raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                        usage: None,
                    })
                    .unwrap(),
                    "application/json",
                    "model_outcomes",
                    "SubjectModelOutcome",
                    2,
                )
                .unwrap()
                .object,
        );
        step.record_revision = RecordRevision(3);
        store.upsert_step(&step, &head, RecordRevision(2)).unwrap();

        let identity = IdentityService::new(Arc::new(shenko_identity::DefaultIdentityPolicy));
        let output = shenko_subject::output::parse_output(step.step_id, object.clone(), model_text, &identity, None)
        .unwrap();
        let mut delegated_task_intents = vec![];
        for control in &output.controls {
            if let shenko_types::output::OutputAction::Delegate { text, definition } =
                &control.action
            {
                delegated_task_intents.push(DelegationIntent {
                    intent_id: task_ids[delegated_task_intents.len()],
                    goal: text.clone(),
                    system_prompt: text.clone(),
                    model: None,
                    capabilities: vec![],
                    budget_hint: None,
                    reply_grant: None,
                    definition: definition.clone(),
                });
            }
        }
        assert_eq!(delegated_task_intents.len(), task_ids.len());
        let next = EntityId::new();
        let decision = StepDecision {
            schema_version: 6,
            output,
            identity_intent: None,
            step_id: step.step_id,
            input_plan_ref: step.input_plan_ref.clone().unwrap(),
            model_result_ref: Some(object),
            applied_steering_refs: vec![],
            event_dispositions,
            delegated_task_intents,
            child_dialogue_intents: vec![],
            memory_intents: vec![],
            policy_intents: vec![],
            publication_intents: vec![],
            pending_obligations: vec![],
            evidence_refs: vec![],
            successor_step_id: next,
        };
        step.decision_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&decision).unwrap(),
                    "application/json",
                    "test",
                    "StepDecision",
                    6,
                )
                .unwrap()
                .object,
        );
        step.state = SubjectStepState::Committed;
        step.record_revision = RecordRevision(4);
        store
            .commit_step_and_advance_head(&step, &head, next)
            .unwrap()
            .commit_ref
    }

    async fn native_fixture_helper(dir: &std::path::Path) -> Result<()> {
        let store = Arc::new(open_native_fixture(dir));
        store.start_subject_dispatcher()?;
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        let _child = supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (store.clone(), RuntimeGeneration::default()),
            )
            .await?;
        let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
            Arc::new(shenko_exec_host::HostPrintfHandler::new(store.clone()));
        let _handler = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
        let native = supervisor
            .context()
            .try_service::<shenko_subject::NativeChildService>()?;
        native.recover()?;
        let agent = store.get_existing_agent_id()?.expect("fixture identity exists");
        let cancel = tokio_util::sync::CancellationToken::new();
        let dispatcher = tokio::spawn(shenko_subject::dispatch::run_committed_dispatcher(
            store.clone(), native, agent, None, cancel.clone(),
        ));
        let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.ready_native_results(agent, 1)?.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        }).await;
        cancel.cancel();
        let dispatched = dispatcher.await;
        supervisor.stop().await?;
        observed??;
        dispatched??;
        Ok(())
    }

    /// Drive bounded dispatcher passes until a durable condition is met. The
    /// dispatcher manages executions asynchronously; asserting "two passes
    /// finish a task" would re-serialize them in the test's favour.
    async fn drive_native_until(
        native: &Arc<shenko_subject::NativeChildService>,
        mut met: impl FnMut() -> Result<bool>,
    ) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                native.run_once().await?;
                if met()? {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await?
    }

    // Real local process boundary: one process executes the committed Pending
    // task and exits; its parent reopens the ledger and must not execute it again.
    // The model output is a deterministic fixture, not production-model evidence.
    #[tokio::test]
    async fn native_printf_result_survives_real_process_reopen() {
        const HELPER: &str = "SHENKO_NATIVE_PRINTF_REOPEN_HELPER";
        if let Some(dir) = std::env::var_os(HELPER) {
            native_fixture_helper(std::path::Path::new(&dir))
                .await
                .unwrap();
            return;
        }
        let dir = std::env::temp_dir().join(format!("shenko-native-reopen-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        let model_profile = shenko_seed::ModelProfile::from_env();
        let agent = bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap();
        store.start_subject_dispatcher().unwrap();
        let printf_task = EntityId::new();
        let missing_task = EntityId::new();
        let natural_task = EntityId::new();
        let expected = "native child 真实结果\nwith spaces";
        let model_text = format!(
            "[delegate:host.printf:1] {}\n[delegate:example.missing:1] {{\"text\":\"missing\"}}\n[delegate] 保留自然语言目标而不伪造执行",
            serde_json::json!({"text":expected,"timeout_secs":5})
        );
        commit_native_fixture_step(
            &store,
            &model_text,
            &[printf_task, missing_task, natural_task],
            vec![],
        );
        let pending = store.external_task(printf_task).unwrap();
        assert!(matches!(pending.state, TaskState::Pending { .. }));
        assert_eq!(pending.kind, "host.printf");
        assert!(store.native_task_result(printf_task).unwrap().is_none());
        drop(store);

        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("recovery_integration_tests::native_printf_result_survives_real_process_reopen")
            .arg("--nocapture")
            .env(HELPER, &dir)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "native helper process failed: {status}");

        let store = Arc::new(open_native_fixture(&dir));
        assert_eq!(
            bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap(),
            agent
        );
        store.start_subject_dispatcher().unwrap();
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        let _child = supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (store.clone(), RuntimeGeneration::default()),
            )
            .await
            .unwrap();
        let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
            Arc::new(shenko_exec_host::HostPrintfHandler::new(store.clone()));
        let _handler = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await
            .unwrap();
        let native = supervisor
            .context()
            .try_service::<shenko_subject::NativeChildService>()
            .unwrap();
        native.recover().unwrap();
        drive_native_until(&native, || {
            Ok(store.native_task_result(printf_task)?.is_some())
        })
        .await
        .unwrap();

        let result = store.native_task_result(printf_task).unwrap().unwrap();
        assert!(matches!(&result.outcome, TaskOutcome::Succeeded));
        assert_eq!(result.task_id, printf_task);
        assert_eq!(
            result
                .result
                .as_ref()
                .and_then(|value| value.get("stdout_utf8"))
                .and_then(serde_json::Value::as_str),
            Some(expected)
        );
        let task = store.external_task(printf_task).unwrap();
        assert!(task.is_terminal());
        assert_eq!(task.updated_commit, result.commit_ref);
        let attempts = store
            .external_operation_attempts(result.operation_id)
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].invocation_id, result.invocation_id);
        let evidence = store
            .external_operation_evidence(result.operation_id)
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].receipt_id, result.receipt_id);
        assert_eq!(
            shenko_store::child::park_reason(&store.external_task(missing_task).unwrap()),
            Some("handler_unavailable")
        );
        assert_eq!(
            shenko_store::child::park_reason(&store.external_task(natural_task).unwrap()),
            Some("definition_unavailable")
        );
        assert!(store.native_task_result(missing_task).unwrap().is_none());
        assert!(store.native_task_result(natural_task).unwrap().is_none());
        assert_eq!(store.ready_native_results(agent, 8).unwrap().len(), 1);

        let inbox = Arc::new(shenko_subject::inbox::Inbox::new(8));
        let cancel = tokio_util::sync::CancellationToken::new();
        let dispatcher = tokio::spawn(shenko_subject::dispatch::run_committed_dispatcher(
            store.clone(),
            native.clone(),
            agent,
            Some(inbox.sender()),
            cancel.clone(),
        ));
        let events = inbox.recv_batch(1, 2_000).await;
        cancel.cancel();
        dispatcher.await.unwrap().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, result.result_id);
        assert_eq!(
            events[0].kind,
            shenko_subject::inbox::InboxEventKind::TaskResult
        );
        assert_eq!(
            events[0]
                .payload
                .get("result")
                .and_then(|value| value.get("stdout_utf8"))
                .and_then(serde_json::Value::as_str),
            Some(expected)
        );
        assert_eq!(
            store
                .external_operation_attempts(result.operation_id)
                .unwrap()
                .len(),
            1
        );
        supervisor.stop().await.unwrap();

        commit_native_fixture_step(
            &store,
            "已接收并核对 child 结果",
            &[],
            vec![EventDisposition {
                event_id: events[0].event_id,
                disposition: Disposition::Acknowledged,
            }],
        );
        assert!(store.ready_native_results(agent, 8).unwrap().is_empty());
        assert_eq!(
            store
                .external_operation_attempts(result.operation_id)
                .unwrap()
                .len(),
            1
        );
        // The process/ledger/evidence directory is retained for readback.
    }

    /// Arm the existing T1 record without spawning a process. This is explicitly
    /// a fault/interleaving simulation; only the preceding test is real process
    /// execution evidence. It verifies fencing and recovery, not SSH or a model.
    fn arm_simulated_native_operation(
        store: &shenko_store::store::Store,
        claim: &shenko_store::NativeTaskClaim,
        implementation: shenko_types::task::ImplementationRef,
    ) -> Result<(
        shenko_types::operation::OperationRecord,
        shenko_types::operation::AttemptRecord,
    )> {
        use shenko_types::{commit::CommitRef, object::*, operation::*};
        let seal =
            |value: &serde_json::Value, schema: &str| -> Result<shenko_types::object::ObjectRef> {
                Ok(store
                    .cas
                    .seal_object(
                        &serde_json::to_vec(value)?,
                        "application/json",
                        "native-fault-simulation",
                        schema,
                        1,
                    )?
                    .object)
            };
        let input_ref = seal(&serde_json::json!({"text":"simulated"}), "input")?;
        let bindings = ResourceBindings {
            provider: "simulation.only".into(),
            account_identity: String::new(),
            runtime_node: claim.invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "no_process_was_started".into(),
            extra: serde_json::json!({"simulation":true}),
        };
        let resource = seal(&serde_json::to_value(bindings)?, "bindings")?;
        let acceptance = seal(
            &serde_json::json!("original simulated result"),
            "acceptance",
        )?;
        let retry = seal(&serde_json::json!("no automatic replay"), "retry")?;
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: implementation.artifact_digest.clone(),
                locator: "simulation:host-printf-v1".into(),
            },
            symbol: "host.printf.execute".into(),
            schema: SchemaRef {
                name: "host-printf-observation-v1".into(),
                major: 1,
                digest: implementation.artifact_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id: claim.operation_id,
            record_revision: RecordRevision(0),
            task_id: claim.task.task_id,
            logical_key: "native-handler".into(),
            operation_name: "host.printf.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: implementation.plugin_id,
                artifact_digest: implementation.artifact_digest.clone(),
                contract_digest: implementation.artifact_digest.clone(),
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: AuthorityRef {
                delegation_id: claim.task.task_id,
                delegation_revision: RecordRevision(claim.invocation.task_revision.0),
                decision_ref: claim
                    .task
                    .created_by_step_id
                    .unwrap_or(claim.invocation.invocation_id),
            },
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let mut attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: claim.operation_id,
            attempt_no: 1,
            invocation_id: claim.invocation.invocation_id,
            worker_id: claim.invocation.worker_id,
            node_id: claim.invocation.node_id,
            epoch: claim.invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        assert!(
            store
                .arm_external_operation(&claim.invocation, &mut operation, &mut attempt)?
                .is_none()
        );
        Ok((operation, attempt))
    }

    #[test]
    fn subject_boundary_keeps_admitted_operation_and_late_evidence() {
        let dir = std::env::temp_dir().join(format!("shenko-native-boundary-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(), ..Default::default()
        }).prepare(store.ledger.ledger_id()).unwrap();
        let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
        store.start_subject_dispatcher().unwrap();
        let admitted = EntityId::new();
        let pending = EntityId::new();
        commit_native_fixture_step(
            &store,
            "[delegate:host.printf:1] {\"text\":\"admitted\"}\n[delegate:host.printf:1] {\"text\":\"pending\"}",
            &[admitted, pending], vec![],
        );
        let descriptor = shenko_subject::NativeTaskHandler::descriptor(
            &shenko_exec_host::HostPrintfHandler::new(store.clone()),
        );
        let claim = store.claim_native_task(admitted, descriptor.implementation_ref.clone(), false).unwrap().unwrap();
        // This exercises the real T1/E1 ledger, not a completed handoff or a
        // claim that an external process ran.
        let (operation, attempt) = arm_simulated_native_operation(&store, &claim, descriptor.implementation_ref.clone()).unwrap();
        let request_id = EntityId::new();
        store.request_subject_quiesce(agent, request_id).unwrap();
        let (head, _) = store.load_subject(agent).unwrap();
        assert!(store.pause_subject_at_boundary(&head).unwrap().is_some());
        assert!(store.claim_native_task(pending, descriptor.implementation_ref.clone(), false).unwrap().is_none());
        assert_eq!(store.external_operation(operation.operation_id).unwrap().state, OperationState::InFlight);

        let raw = store.cas.seal_object(b"late evidence", "text/plain", "test", "evidence", 1).unwrap().object;
        let evidence = shenko_types::evidence::EvidenceReceipt {
            record_schema_version: 1, receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id), attempt_id: Some(attempt.attempt_id),
            source_id: "boundary.fixture".into(), source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some("late".into()), producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()), raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: shenko_types::evidence::DecodeStatus::Decoded,
            observation_level: Some(shenko_types::evidence::ObservationLevel::PostconditionVerified),
            normalized_ref: None, observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        let recorded = store.record_external_evidence(operation.operation_id, attempt.attempt_id, evidence,
            Some(OperationOutcome::Succeeded), EffectStatus::None).unwrap();
        assert_eq!(recorded.state, OperationState::Resolved);
        assert_eq!(store.external_operation_attempts(operation.operation_id).unwrap().len(), 1);
        store.resume_subject_quiesce(agent, request_id).unwrap();
        assert!(store.claim_native_task(pending, descriptor.implementation_ref, false).unwrap().is_some());
    }

    #[tokio::test]
    async fn native_cancel_late_evidence_and_unknown_recovery_are_fenced() {
        let dir = std::env::temp_dir().join(format!("shenko-native-fencing-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let old_store = Arc::new(open_native_fixture(&dir));
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(old_store.ledger.ledger_id())
        .unwrap();
        bootstrap_genesis(
            &old_store,
            None,
            &prepared_home.manifest,
            &shenko_seed::ModelProfile::from_env(),
        ).unwrap();
        old_store.start_subject_dispatcher().unwrap();
        let cancelled_task = EntityId::new();
        let unknown_task = EntityId::new();
        commit_native_fixture_step(
            &old_store,
            "[delegate:host.printf:1] {\"text\":\"cancel\"}\n[delegate:host.printf:1] {\"text\":\"unknown\"}",
            &[cancelled_task, unknown_task],
            vec![],
        );
        let descriptor = shenko_subject::NativeTaskHandler::descriptor(
            &shenko_exec_host::HostPrintfHandler::new(old_store.clone()),
        );
        let cancelled_claim = old_store
            .claim_native_task(cancelled_task, descriptor.implementation_ref.clone(), false)
            .unwrap()
            .unwrap();
        let unknown_claim = old_store
            .claim_native_task(unknown_task, descriptor.implementation_ref.clone(), false)
            .unwrap()
            .unwrap();
        let (cancelled_operation, cancelled_attempt) = arm_simulated_native_operation(
            &old_store,
            &cancelled_claim,
            descriptor.implementation_ref.clone(),
        )
        .unwrap();
        let (_unknown_operation, _unknown_attempt) = arm_simulated_native_operation(
            &old_store,
            &unknown_claim,
            descriptor.implementation_ref.clone(),
        )
        .unwrap();
        let old_native = shenko_subject::NativeChildService::new(
            old_store.clone(),
            RuntimeGeneration::default(),
        );
        assert_eq!(
            old_native
                .cancel(
                    cancelled_task,
                    "test cancellation",
                    AbortScope::SubtreeOrdinary
                )
                .unwrap()
                .cancelled,
            vec![cancelled_task]
        );

        // A late original result is durable E1, not authority for the cancelled
        // invocation to create T2 or a subject result.
        let raw = old_store
            .cas
            .seal_object(
                br#"{"simulated_late_result":true}"#,
                "application/json",
                "native-fault-simulation",
                "late-result",
                1,
            )
            .unwrap()
            .object;
        let late = shenko_types::evidence::EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(cancelled_operation.operation_id),
            attempt_id: Some(cancelled_attempt.attempt_id),
            source_id: "simulation.original".into(),
            source_instance_id: cancelled_attempt.node_id.to_string(),
            source_event_key: Some("late-original-result".into()),
            producer_epoch: Some(cancelled_attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(cancelled_operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: shenko_types::evidence::DecodeStatus::Decoded,
            observation_level: Some(
                shenko_types::evidence::ObservationLevel::PostconditionVerified,
            ),
            normalized_ref: None,
            observed_at: Some(chrono::Utc::now()),
            received_commit: cancelled_operation.prepared_commit,
        };
        let resolved = old_store
            .record_external_evidence(
                cancelled_operation.operation_id,
                cancelled_attempt.attempt_id,
                late,
                Some(OperationOutcome::Succeeded),
                EffectStatus::None,
            )
            .unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert!(
            old_store
                .complete_native_task(&cancelled_claim, &resolved)
                .unwrap()
                .is_none()
        );
        assert!(
            old_store
                .native_task_result(cancelled_task)
                .unwrap()
                .is_none()
        );

        let reopened = Arc::new(open_native_fixture(&dir));
        reopened.start_subject_dispatcher().unwrap();
        assert!(
            old_store
                .complete_native_task(&cancelled_claim, &resolved)
                .is_err()
        );
        drop(old_native);
        drop(old_store);

        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        let _child = supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (reopened.clone(), RuntimeGeneration::default()),
            )
            .await
            .unwrap();
        let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
            Arc::new(shenko_exec_host::HostPrintfHandler::new(reopened.clone()));
        let _handler = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await
            .unwrap();
        let native = supervisor
            .context()
            .try_service::<shenko_subject::NativeChildService>()
            .unwrap();
        native.recover().unwrap();
        drive_native_until(&native, || {
            let task = reopened.external_task(unknown_task)?;
            Ok(matches!(task.state, TaskState::Pending { .. })
                && shenko_store::child::park_reason(&task) == Some("reconciling"))
        })
        .await
        .unwrap();

        let unknown = reopened
            .external_operation(unknown_claim.operation_id)
            .unwrap();
        assert_eq!(unknown.state, OperationState::Reconciling);
        assert_eq!(unknown.effect_status, EffectStatus::Unknown);
        assert_eq!(unknown.outcome, None);
        let unknown_task_record = reopened.external_task(unknown_task).unwrap();
        assert!(matches!(
            unknown_task_record.state,
            TaskState::Pending { .. }
        ));
        assert_eq!(
            shenko_store::child::park_reason(&unknown_task_record),
            Some("reconciling")
        );
        assert_eq!(
            reopened
                .external_operation_attempts(unknown_claim.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_evidence(unknown_claim.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert!(reopened.native_task_result(unknown_task).unwrap().is_none());
        assert!(
            reopened
                .native_task_result(cancelled_task)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reopened
                .external_operation_attempts(cancelled_operation.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_evidence(cancelled_operation.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert!(
            reopened
                .external_task(cancelled_task)
                .unwrap()
                .abort_request
                .is_some()
        );
        supervisor.stop().await.unwrap();
        // Simulated fault artifacts and all original receipts remain on disk.
    }
}

#[cfg(test)]
mod reply_directive_tests {
    use shenko_types::{ids::EntityId, output::*};
    #[test]
    fn identity_lines_are_actions_not_reply_text() {
        let out = "先想一下\n[reply:group:7]  你好\n[identity:name] 岚\n继续说\n[identity:soul] 记住关系；不要把 delegate 和委派当指令\n[remember] 一整段记忆\n[delegate] 完整有限目标\n> [delegate] 引用，不执行\n[remember] 懒续行仍是引用\n\n[identity:personality] 这是链接旁的说明\n\n[identity:personality]: https://example.invalid/\n\n[delegate](https://example.invalid/)\n[1] 引用编号不是控制行\n```text\n[identity:name] 代码，不执行\n[remember] 代码例子\n```\n[reply:private:9] 第二条";
        let dir = std::env::temp_dir().join(format!("shenko-controls-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = shenko_store::store::Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
        let raw = store
            .cas
            .seal_object(out.as_bytes(), "text/plain", "test", "text", 1)
            .unwrap()
            .object;
        let identity = shenko_identity::IdentityService::new(std::sync::Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ));
        let parsed =
            shenko_subject::output::parse_output(EntityId::new(), raw.clone(), out, &identity, None)
                .unwrap();
        assert_eq!(parsed.controls.len(), 6); // two replies, two identity, one memory, one task
        let first = &parsed.controls[0];
        let OutputAction::Reply { target, text } = &first.action else {
            panic!("reply")
        };
        assert_eq!(*target, PublicationTarget::QqGroup(7));
        assert_eq!(
            text,
            " 你好\n继续说\n> [delegate] 引用，不执行\n[remember] 懒续行仍是引用\n\n[identity:personality] 这是链接旁的说明\n\n[identity:personality]: https://example.invalid/\n\n[delegate](https://example.invalid/)\n[1] 引用编号不是控制行\n```text\n[identity:name] 代码，不执行\n[remember] 代码例子\n```\n"
        );
        assert!(!parsed.text.contains("一整段记忆") && !parsed.text.contains("完整有限目标"));
        assert_eq!(
            &out[first.spans[0].start..first.spans[0].end],
            "[reply:group:7]  你好\n"
        );
        assert_eq!(
            store.cas.read_object(&parsed.raw_ref.digest).unwrap(),
            out.as_bytes()
        );
        // Unsupported old syntax is a diagnosed protocol error, not a fallback.
        let obsolete = store
            .cas
            .seal_object(b"[memory] obsolete", "text/plain", "test", "text", 1)
            .unwrap()
            .object;
        assert!(
            shenko_subject::output::parse_output(
                EntityId::new(),
                obsolete,
                "[memory] obsolete",
                &identity,
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("CONTROL_PROTOCOL_INVALID")
        );
    }

    /// The semantic wait protocol is one typed control per result.
    ///
    /// Malformed or duplicate wait controls reject the whole result, while
    /// a quoted or fenced wait line remains literal prose.
    #[test]
    fn semantic_wait_controls_are_typed_and_atomic() {
        use shenko_types::subject::SemanticWaitChange;

        let dir = std::env::temp_dir().join(format!("shenko-wait-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = shenko_store::store::Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
        let identity = shenko_identity::IdentityService::new(std::sync::Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ));
        let parse = |raw: &str| {
            let raw_ref = store
                .cas
                .seal_object(raw.as_bytes(), "text/plain", "test", "text", 1)
                .unwrap()
                .object;
            shenko_subject::output::parse_output(EntityId::new(), raw_ref, raw, &identity, None)
        };

        // A start names the condition and its cap, next to an ordinary action.
        let started = parse(
            "先想清楚再等\n[wait] {\"condition\":\"实验出现足够判断的结果\",\"max_wait_secs\":120}\n[remember] 记住等待条件",
        )
        .unwrap();
        assert_eq!(started.schema_version, 3);
        assert_eq!(started.controls.len(), 2);
        let OutputAction::SemanticWait { change } = &started.controls[0].action else {
            panic!("the wait control must be a typed semantic wait action")
        };
        assert_eq!(
            change,
            &SemanticWaitChange::Start {
                condition: "实验出现足够判断的结果".into(),
                max_wait_secs: 120,
            }
        );
        assert!(!started.text.contains("max_wait_secs"));

        // An out-of-range cap rejects the whole result, not just this action.
        let invalid = parse("[remember] 先记下\n[wait] {\"condition\":\"x\",\"max_wait_secs\":601}");
        assert!(invalid.unwrap_err().to_string().contains("CONTROL_PROTOCOL_INVALID"));

        // One wait control per result: a second one rejects the whole result.
        let duplicate = parse(
            "[wait] {\"condition\":\"x\",\"max_wait_secs\":10}\n[wait] {\"condition\":\"y\",\"max_wait_secs\":10}",
        );
        assert!(
            duplicate
                .unwrap_err()
                .to_string()
                .contains("CONTROL_PROTOCOL_INVALID")
        );

        // A quoted wait line is prose, not a control.
        let quoted = parse("> [wait] {\"condition\":\"引用不执行\",\"max_wait_secs\":10}\n").unwrap();
        assert!(quoted.controls.is_empty());
        assert!(quoted.text.contains("[wait] {\"condition\":\"引用不执行\""));

        // A fenced wait line is prose, not a control.
        let fenced = parse("```text\n[wait:end] 代码不执行\n```\n")
        .unwrap();
        assert!(fenced.controls.is_empty());
        assert!(fenced.text.contains("[wait:end]"));
    }
}
