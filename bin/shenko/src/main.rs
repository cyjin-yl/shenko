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
            let (broker, _broker_handle) = shenko_broker::broker::Broker::new();
            tokio::spawn(broker.run());

            // 4. Start subject loop — the continuous main agent. SPEC §5.1
            let cancel = tokio_util::sync::CancellationToken::new();
            let subject_inbox = Arc::new(shenko_subject::inbox::Inbox::new(1024));
            let dispatcher = Arc::new(shenko_subject::dispatch::ChildDispatcher::new(agent_id));
            let loop_state = shenko_subject::loop_runner::SubjectLoopState::new(
                agent_id,
                Arc::clone(&subject_inbox),
                Arc::clone(&dispatcher),
            ).with_store(Arc::clone(&store));

            let cancel_subject = cancel.clone();
            tokio::spawn(shenko_subject::loop_runner::run_subject_loop(loop_state, cancel_subject));
            info!("subject loop started");

            // 5. Start NapCat if configured — SPEC §16
            if let Some(ws_url) = napcat {
                let napcat_config = shenko_channel_napcat::config::NapCatConfig {
                    ws_url,
                    ..Default::default()
                };
                let (_, _event_rx, _action_rx) = shenko_channel_napcat::client::NapCatClient::new(napcat_config.clone());
                let cancel_napcat = cancel.clone();
                let inbox_tx = subject_inbox.sender();
                tokio::spawn(async move {
                    // Route NapCat events to subject inbox
                    let (event_tx_inner, mut event_rx_inner) = tokio::sync::mpsc::channel(256);
                    let (_action_tx_inner, action_rx_inner) = tokio::sync::mpsc::channel(256);
                    tokio::spawn(shenko_channel_napcat::client::NapCatClient::run_connection_loop(
                        napcat_config, event_tx_inner, action_rx_inner, cancel_napcat.clone(),
                    ));
                    while let Some(ev) = event_rx_inner.recv().await {
                        if cancel_napcat.is_cancelled() { break; }
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
                info!("NapCat connector started");
            }

            // 6. Start WebUI
            let ui = shenko_ui::server::UiServer::new(port);
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

/// Ensure genesis exists for this installation. Returns agent_id. SPEC §44.6
fn bootstrap_genesis(
    store: &shenko_store::store::Store,
    name: Option<&str>,
) -> Result<shenko_types::ids::EntityId> {
    use shenko_types::ids::*;
    use shenko_types::subject::*;
    use shenko_types::commit::CommitRef;
    use shenko_types::object::{ObjectRef, SchemaRef, Digest};

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

    // Stub object refs (real impls would seal actual manifests)
    let stub_codec = SchemaRef { name: "json".into(), major: 1, digest: Digest::blake3(b"json") };
    let stub_obj = |data: &[u8]| {
        let d = Digest::blake3(data);
        ObjectRef { digest: d, bytes: data.len() as u64, media_type: "application/json".into(), store_namespace: "bootstrap".into(), locator: String::new(), codec: stub_codec.clone() }
    };

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id,
        launch_id,
        seed_manifest_ref: stub_obj(b"seed_manifest"),
        initial_profile_ref: stub_obj(b"initial_profile"),
        home_manifest_ref: stub_obj(b"home_manifest"),
        bootstrap_event_ref: stub_obj(b"bootstrap_event"),
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
