//! Composed live startup and subject/UI supervision for desktop and embedded hosts.
use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use shenko_identity::IdentityService;
use tracing::info;

use crate::{node_sync, runtime_channels, runtime_cognition, runtime_inference, runtime_publications, update_control, update_runtime};
use crate::runtime_bootstrap::{adopt_identity, bootstrap_genesis, create_data_directory_async};
use crate::runtime_consumers::RuntimeConsumers;
use crate::runtime_lifecycle::{StoreErrorSink, spawn_runtime_hook_pump};
use crate::runtime_plugins::{install_native_definitions, install_native_ssh, spawn_media_services};

pub(crate) struct StartConfig {
    pub(crate) port: u16,
    pub(crate) pause_at_start: Option<shenko_types::ids::EntityId>,
    pub(crate) name: Option<String>,
    pub(crate) channels: runtime_channels::ChannelConfig,
    pub(crate) ssh_targets: Option<PathBuf>,
}

pub(crate) async fn start(
    supervisor: &shenko_kernel::Supervisor,
    identity_context: &cordis_core::Context,
    identity_service: &IdentityService,
    runtime_state: &mut crate::runtime_lifecycle::RuntimeState,
    data_dir: PathBuf,
    config: StartConfig,
) -> Result<()> {
    let StartConfig { port, pause_at_start, name, channels, ssh_targets } = config;
    if channels.napcat.napcat.is_some() {
        anyhow::ensure!(
            channels.napcat.napcat_account.is_some_and(|account| account > 0),
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
        napcat_ws_url: channels.napcat.napcat.clone(),
    });
    bootstrap.validate_target()?;
    let model_profile = shenko_seed::ModelProfile::from_env();

    // 1. Open persistent store and prepare the actual WASI home.
    //    The store lives in the live slot the durable pointer names
    //    (`versions/<gen>/`); a data dir without any slot published
    //    yet is itself the live slot (first run).
    create_data_directory_async(&data_dir).await?;
    let live_slot = shenko_update::slots::resolve_live_slot(&data_dir)?
        .unwrap_or_else(|| data_dir.clone());
    let db_path = live_slot.join(shenko_update::SLOT_DB_FILENAME);
    // The content store is shared by all generations.
    let cas_dir = data_dir.join("cas");
    let store = Arc::new(shenko_store::store::Store::open(db_path, cas_dir)?);
    info!(
        sqlite=%store.capabilities.sqlite_version,
        wal=store.capabilities.wal_mode,
        fts5=store.capabilities.fts5_available,
        "store ready"
    );

    let installation_id = match store.get_existing_agent_id()? {
        Some(agent_id) => store.installation_id_for_subject(agent_id)?,
        None => store.ledger.ledger_id(),
    };
    // Prepare the protected installation control channel before home
    // setup, connectors, or subject work can publish runtime effects.
    // Claiming is not a prerequisite for ordinary conversation.
    if let Some(path) = shenko_ui::owner::prepare_owner_claim(
        &store,
        &data_dir,
        installation_id,
    )? {
        info!(control_file=%path.display(), "owner claim available through the private installation control file");
    }
    let owner_state = shenko_ui::owner::OwnerState {
        store: Arc::clone(&store),
        installation_id,
        data_dir: data_dir.clone(),
    };
    let prepared_home = bootstrap.prepare_with_recovery(installation_id, &store.cas)?;
    info!(manifest=%prepared_home.manifest_path.display(), "WASM home ready");
    for recovery in &prepared_home.recovered {
        tracing::warn!(%recovery, "completed home recovery");
    }

    // 2. Bootstrap genesis if first run. The genesis
    // closure references the actual home manifest and guest artifact.
    let agent_id = bootstrap_genesis(
        &store,
        name.as_deref(),
        &prepared_home.manifest,
        &model_profile,
    )?;
    // kernel cordis-lifecycle and hook-graph failures journal
    // into the SAME subject store journal. Genesis is the earliest
    // truthful attach point — before it no branch exists, and the
    // kernel keeps its stderr+counter record instead of manufacturing
    // one.
    supervisor.attach_error_sink(Arc::new(StoreErrorSink {
        store: Arc::clone(&store),
        agent_id,
    }));
    // The wasi.exec handler keeps the mount closure and the data dir;
    // per-execution it reopens this installation's unit inventory so
    // installs made after startup are visible.
    let wasi_mounts = prepared_home.manifest.mounts.clone();
    drop(prepared_home);
    let identity = adopt_identity(&store, agent_id, name.as_deref(), identity_service)?;
    info!(
        revision = identity.revision,
        name = identity.display_name.as_deref().unwrap_or("(unnamed)"),
        naming = ?identity.naming,
        "identity ready"
    );

    let runtime_instance = shenko_types::ids::EntityId::new();
    // The launch binds this process to the durable control
    // authority (the activation selector) before any subject step.
    let launch_receipt =
        shenko_seed::LaunchService::new(Arc::clone(&store), agent_id).resolve(
            &shenko_seed::LaunchRequest {
                launch_id: runtime_instance,
                installation_id,
                agent_id,
                slot_dir: live_slot.clone(),
                reason: "start".to_string(),
                read_only: false,
            },
        )?;
    info!(
        generation = %launch_receipt.generation.0,
        selector_revision = launch_receipt.selector_revision,
        selector_epoch = launch_receipt.selector_epoch,
        "launch bound to control authority"
    );
    // Deterministic recovery services remain available without an LLM.
    let cancel = tokio_util::sync::CancellationToken::new();
    let _runtime_cancel_on_exit = cancel.clone().drop_guard();
    // Establish the actual branch before constructing any scoped
    // registry or dispatch consumer, including model-free services.
    let (primary_fence, authority_runtime) = match node_sync::establish_runtime(
        &data_dir, &store, &cancel, runtime_instance, &model_profile,
    ).await? {
        node_sync::RuntimeSelection::Selected { fence, authority, heartbeat } => {
            runtime_state.heartbeat = Some(heartbeat);
            (fence, authority)
        },
        node_sync::RuntimeSelection::Stopped => return Ok(()),
    };
    runtime_state.authority = Some(authority_runtime.clone());
    authority_runtime.activate()?;
    if let Some(request_id) = pause_at_start {
        let boundary = store.pause_subject_at_start(agent_id, request_id)?;
        runtime_state.startup_pause_applied = true;
        info!(%request_id, boundary=?boundary.boundary_at,
            "startup paused at a durable subject boundary; use runtime resume to continue");
    }
    let subject_inbox = Arc::new(shenko_subject::inbox::Inbox::new(1024));
    // Establish recovery consumers BEFORE ResultRecorded can take fast S1.
    store.start_subject_dispatcher()?;
    store.settle_subject_memory()?;
    store.reconcile_subject_publications()?;
    // Typed continuous-media services first: one durable revision
    // admission + the bound subject inbox event, plus the live runtime
    // behind the reserved slots (realtime sessions against configured
    // engine bindings in data_dir/media/providers.json, recording
    // capture with CAS closure, barge-in, speech playback and speaker
    // identification). They are published before the
    // native definitions that bind to them, and the /api/media routes
    // share the same instances. No engine configured stays honest: the
    // ingress path still admits external provider facts, and an engine
    // call reports a typed refusal.
    let media = spawn_media_services(
        supervisor,
        &store,
        &data_dir,
        agent_id,
        subject_inbox.sender(),
    )
    .await?;
    // Native finite children use the same Cordis root and persistent
    // ledger. Fixed native definitions retain their original handlers;
    // the finite model handler is installed with the shared broker below.
    let installed = install_native_definitions(
        supervisor,
        &store,
        &data_dir,
        wasi_mounts.clone(),
        &media,
    )
    .await?;
    installed.playground_state.with_inbox(subject_inbox.sender());
    if let Some(path) = ssh_targets {
        let ssh_node = node_sync::open_node(&data_dir, &store)?;
        install_native_ssh(supervisor, Arc::clone(&store), ssh_node, &path).await?;
        info!("native SSH explicit installation ready; credentials are loaded only after task admission");
    }
    // The runtime's own changes become hook events from now on: the
    // installation above and every later registration are observed
    // through the live graph, not through a startup snapshot.
    let _runtime_hook_pump = spawn_runtime_hook_pump(supervisor, &cancel);
    let native_child = installed.native_child.clone();
    let self_knowledge = installed.self_knowledge.clone();
    let handoff_participants = Arc::clone(&installed.handoff_participants);
    // Recovery belongs to the live runtime, never to capability discovery.
    native_child.recover()?;
    installed.artifact_services.recover_on_boot()?;
    let _update_control = update_control::spawn_control(
        data_dir.clone(),
        Arc::clone(&store),
        Arc::clone(&handoff_participants),
        cancel.clone(),
    )
    .await?;
    // Resident memory worker: accepted ≠ remembered.
    // It drains the durable memory intent queue and keeps the Git
    // text knowledge repository (data_dir/memory) in sync. The
    // repository is initialized here when the seed bootstrap has
    // not run yet (a copied data dir recovers the same way).
    let memory_knowledge = Arc::new(
        shenko_memory::GitKnowledgeStore::open(data_dir.join("memory"))?,
    );
    let _memory_worker_plugin = supervisor
        .spawn(
            shenko_memory::MemoryWorkerPlugin,
            (
                agent_id,
                Arc::clone(&store),
                Some(memory_knowledge),
            ),
        )
        .await?;
    let cognition = runtime_cognition::install(
        supervisor, &store, &data_dir, &native_child, &self_knowledge,
    ).await?;
    let consumers = RuntimeConsumers::spawn(
        &store, &native_child, agent_id,
        model_profile.configured.then(|| subject_inbox.sender()),
        &data_dir, &cancel,
    );
    let consumer_cancel = &consumers.cancel;
    if !model_profile.configured {
        node_sync::observe(&data_dir, &store, runtime_instance, primary_fence.as_ref())?;
        if channels.napcat.napcat.is_some() {
            tracing::warn!("NapCat not started without a subject model; durable bootstrap remains available");
        }
        let owner_grant =
            store.get_bootstrap_owner_grant(installation_id)?;
        let view = bootstrap.view(
            installation_id,
            shenko_seed::BootstrapSubjectState::ExistingGenesis,
            model_profile.clone(),
            owner_grant,
        );
        println!("Bootstrap: {}", serde_json::to_string_pretty(&view)?);
        let runtime_result =
            run_deterministic_ui(port, store, data_dir, model_profile, cancel, native_child, owner_state).await;
        consumers.stop(&authority_runtime).await?;
        return runtime_result;
    }

    let runtime_inference::InferenceServices { context_registry, broker_handle, capabilities } =
        runtime_inference::install(runtime_inference::InferenceInstall {
            supervisor, store: &store, data_dir: &data_dir, agent_id,
            subject_inbox: &subject_inbox, cognition: &cognition,
            installed: &installed, cancel: &cancel, consumer_cancel,
        }).await?;
    // The hook graph is the runtime's own service: the subject and
    // every plugin resolve the same live registry, so a handler a
    // plugin registered is visible to the subject's next step.
    let hook_registry = Arc::clone(&supervisor.hooks);
    // Outbound path: subject model output → UI timeline.
    let (outbox_tx, _outbox_rx) =
        tokio::sync::broadcast::channel::<shenko_subject::outbox::OutboxMessage>(64);
    let channel_services = runtime_channels::install(runtime_channels::ChannelInstall {
        supervisor, store: &store, data_dir: &data_dir, agent_id,
        subject_inbox: &subject_inbox, self_knowledge: &self_knowledge,
        consumer_cancel, config: channels,
    }).await?;
    runtime_publications::spawn(
        &channel_services.registry, &store, agent_id, consumer_cancel, &cancel,
    );

    // Built after any connector-scoped handler install so the sealed
    // tool contract advertises exactly the installed definitions.
    // The provider stream: a durable journal for the stream itself
    // and the reader surface the WebUI watches. The subject keeps the
    // ordinary path when no journal is wired, and the UI then
    // honestly reports that streaming is unavailable.
    let stream_sink = shenko_ui::stream::StreamSink::new(
        Arc::clone(&store),
        agent_id,
        256,
    );
    let stream_journal = Arc::new(shenko_subject::SubjectStreamJournal::new(
        Arc::clone(&store),
        agent_id,
    ));
    let stream_public: Arc<dyn shenko_broker::stream::PublicStreamSink> = Arc::new(
        shenko_ui::stream::UiPublicStreamSink::new(Arc::clone(&stream_sink)),
    );
    let loop_state = shenko_subject::loop_runner::SubjectLoopState::new(
        agent_id,
        Arc::clone(&subject_inbox),
        Arc::clone(&store),
        identity_context.clone(),
        channel_services.registry,
    )?
    .with_broker(broker_handle)
    .with_model_capabilities(capabilities.clone())
    .with_native_capabilities(native_child.installed_prompt_sections())
    .with_awareness(cognition)
    .with_hooks(Arc::clone(&hook_registry))
    .with_outbox(outbox_tx.clone())
    .with_stream_sinks(stream_journal, Some(stream_public));
    let reserved_output_tokens = loop_state.max_tokens;

    // Connector/ACK storage failures also stop admission in the subject.
    let connector_failure = cancel.clone();
    let connector_stop = consumer_cancel.clone();
    tokio::spawn(async move { connector_stop.cancelled().await; connector_failure.cancel(); });
    let cancel_subject = cancel.clone();
    let subject_driver = update_runtime::SubjectDriver::new(Arc::clone(&handoff_participants));
    let mut subject_task = tokio::spawn(async move {
        let _subject_driver = subject_driver;
        shenko_subject::loop_runner::run_subject_loop(loop_state, cancel_subject).await
    });
    node_sync::observe(&data_dir, &store, runtime_instance, primary_fence.as_ref())?;
    info!("subject loop started");

    // The configuration service installed with the native definitions:
    // discover/read/preview/apply over the durable config tables, the
    // very same instance the subject's `config.manage` tool drives.
    // Namespaces register their handlers on this service; without a
    // handler a namespace is not advertised.
    let config_service = Arc::clone(&installed.config_service);

    // 7. Start WebUI
    let ui = shenko_ui::server::UiServer::new(port)
        .with_native(Arc::clone(&native_child))
        .with_inbox(subject_inbox.sender())
        .with_outbox(outbox_tx)
        .with_stream_sink(Arc::clone(&stream_sink))
        .with_store(Arc::clone(&store))
        .with_media_ingress(Arc::clone(&media.ingress))
        .with_media_runtime(Arc::clone(&media.runtime))
        .with_context(Arc::clone(&context_registry), capabilities, reserved_output_tokens)
        .with_data_dir(data_dir.clone())
        .with_owner(owner_state)
        .with_config(config_service)
        .with_artifact_services(Arc::clone(&installed.artifact_services))
        .with_playground_state(Arc::clone(&installed.playground_state))
        .with_inference_status(model_profile.model.clone(), model_profile.configured);
    println!("Shenko UI: http://{}", ui.bind);
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
    // Signal expected delivery shutdown before closing the receiver.
    // Durable consumers retain their separate settlement lifetime.
    cancel.cancel();
    subject_inbox.close().await;
    if !subject_finished {
        // Do not drop an admitted request merely because Ctrl-C arrived.
        // A timeout is an observation limit, never proof the provider stopped.
        match tokio::time::timeout(std::time::Duration::from_secs(15), &mut subject_task)
            .await
        {
            Ok(Ok(Ok(()))) => {},
            Ok(Ok(Err(error))) => {
                consumer_cancel.cancel();
                return Err(error.into());
            },
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => {
                consumer_cancel.cancel();
                let (_, step) = store.load_subject(agent_id)?;
                return Err(anyhow::anyhow!(
                    "shutdown settlement timed out: durable step {} is {:?}, request {:?}; remote outcome is NOT proven stopped, reopen/reconcile before dispatch",
                    step.step_id,
                    step.state,
                    step.active_request_id
                ));
            }
        }
    }
    // S1 has settled or left a durable unresolved step. Do not drop the
    // consumers first. Their unfinished work remains queryable on reopen.
    consumers.stop(&authority_runtime).await?;
    runtime_result?;
    info!("Shenko stopped");
    Ok(())
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
    native: Arc<shenko_subject::NativeChildService>,
    owner: shenko_ui::owner::OwnerState,
) -> Result<()> {
    let ui = shenko_ui::server::UiServer::new(port)
        .with_native(native)
        .with_store(store)
        .with_data_dir(data_dir)
        .with_owner(owner)
        .with_inference_status(model_profile.model, model_profile.configured);
    println!("Shenko UI: http://{}", ui.bind);
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
