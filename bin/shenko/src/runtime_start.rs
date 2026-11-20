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
    state: &mut crate::runtime_lifecycle::RuntimeState,
    data_dir: PathBuf,
    config: StartConfig,
) -> Result<()> {
    let StartConfig { port, pause_at_start, name, channels, ssh_targets } = config;
    let _ = channels.napcat.configured_account()?;
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
    let store = Arc::new(crate::runtime_bootstrap::open_installation_store(&data_dir)?);
    info!(
        sqlite=%store.capabilities.sqlite_version,
        wal=store.capabilities.wal_mode,
        fts5=store.capabilities.fts5_available,
        "store ready"
    );

    let installation_id = store.installation_id_for_home()?;
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
    prepared_home.record_local_provisioning(&store)?;
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
    // record_local_provisioning has validated this installation's ready home.
    // Capture the same closure once; Files never reparses host path authority.
    let file_mounts = Arc::new(shenko_ui::files::FileMounts::from_validated_startup(&wasi_mounts)?);
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
    let model_health = tokio::select! {
        _result = crate::runtime_model_health::initial_probe(
            &model_profile, Arc::clone(&store), &cancel,
        ) => _result?,
        _result = tokio::signal::ctrl_c() => {
            _result?;
            cancel.cancel();
            return Ok(());
        },
    };
    state.model_health_monitor = Some(model_health.spawn_monitor(cancel.clone()));
    // Establish the actual branch before constructing any scoped
    // registry or dispatch consumer, including model-free services.
    let (primary_fence, authority_runtime) = match node_sync::establish_runtime(
        &data_dir, &store, &cancel, runtime_instance, &model_profile, model_health.eligibility(),
    ).await? {
        node_sync::RuntimeSelection::Selected { fence, authority, heartbeat } => {
            state.authority_heartbeat = Some(heartbeat);
            (fence, authority)
        },
        node_sync::RuntimeSelection::Stopped => return Ok(()),
        node_sync::RuntimeSelection::RecoveryOnly { heartbeat, reason } => {
            state.authority_heartbeat = Some(heartbeat);
            return run_deterministic_ui(supervisor, DeterministicUi {
                port, store, data_dir, agent_id, runtime_instance, model_profile,
                model_health, cancel, owner: owner_state, channels, reason, file_mounts,
            }).await;
        },
    };
    state.active_authority = Some(authority_runtime.clone());
    authority_runtime.bind_model_health(Arc::clone(&model_health), runtime_instance);
    authority_runtime.activate()?;
    if let Some(request_id) = pause_at_start {
        let boundary = store.pause_subject_at_start(agent_id, request_id)?;
        state.startup_pause_applied = true;
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
    state.hook_pump = Some(spawn_runtime_hook_pump(supervisor, &cancel));
    let native_child = installed.native_child.clone();
    state.native_child = Some(Arc::clone(&native_child));
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

    let inference = runtime_inference::install(runtime_inference::InferenceInstall {
        supervisor,
        store: &store,
        data_dir: &data_dir,
        agent_id,
        subject_inbox: &subject_inbox,
        cognition: &cognition,
        installed: &installed,
        cancel: &cancel,
        consumer_cancel,
        model_health: &model_health,
    })
    .await;
    let runtime_inference::InferenceServices {
        context_registry,
        broker_handle,
        capabilities,
        task: inference_task,
    } =
        match inference {
            Ok(Some(services)) => services,
            stopped => {
                cancel.cancel();
                let result = stopped.map(|_| ());
                return match consumers.stop(&authority_runtime).await {
                    Ok(()) => result,
                    Err(cleanup) => match result {
                        Ok(()) => Err(cleanup),
                        Err(error) => Err(error
                            .context(format!("startup consumers also failed to stop: {cleanup}"))),
                    },
                };
            }
        };
    // The hook graph is the runtime's own service: the subject and
    // every plugin resolve the same live registry, so a handler a
    // plugin registered is visible to the subject's next step.
    state.inference = Some(inference_task);
    let hook_registry = Arc::clone(&supervisor.hooks);
    // Outbound path: subject model output → UI timeline.
    let (outbox_tx, _outbox_rx) =
        tokio::sync::broadcast::channel::<shenko_subject::outbox::OutboxMessage>(64);
    let channel_services = runtime_channels::install(runtime_channels::ChannelInstall {
        supervisor, store: &store, data_dir: &data_dir, agent_id,
        subject_inbox: &subject_inbox, self_knowledge: Some(&self_knowledge),
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
    // Incarnations reopen from durable facts; their failure does not own the
    // ingress, native-worker or UI lifetimes.
    let make_subject = {
        let store = Arc::clone(&store);
        let inbox = Arc::clone(&subject_inbox);
        let identity_context = identity_context.clone();
        let channel_registry = channel_services.registry.clone();
        let capabilities = capabilities.clone();
        let native_child = Arc::clone(&native_child);
        let outbox_tx = outbox_tx.clone();
        move || Ok(shenko_subject::loop_runner::SubjectLoopState::new(
            agent_id, Arc::clone(&inbox), Arc::clone(&store),
            identity_context.clone(), channel_registry.clone(),
        )?
        .with_broker(broker_handle.clone())
        .with_model_capabilities(capabilities.clone())
        .with_native_capabilities(native_child.installed_prompt_sections())
        .with_awareness(Arc::clone(&cognition))
        .with_hooks(Arc::clone(&hook_registry))
        .with_outbox(outbox_tx.clone())
        .with_stream_sinks(stream_journal.clone(), Some(stream_public.clone())))
    };
    let reserved_output_tokens = shenko_subject::loop_runner::DEFAULT_MAX_OUTPUT_TOKENS;

    // Connector/ACK storage failures also stop admission in the subject.
    let connector_failure = cancel.clone();
    let connector_stop = consumer_cancel.clone();
    tokio::spawn(async move { connector_stop.cancelled().await; connector_failure.cancel(); });
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
        .with_file_mounts(file_mounts)
        .with_file_peers(file_peers(&data_dir).ok())
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
    let subject_live = Arc::clone(&live);
    let cancel_subject = cancel.clone();
    let subject_store = Arc::clone(&store);
    let subject_driver = update_runtime::SubjectDriver::new(Arc::clone(&handoff_participants));
    let mut subject_task = tokio::spawn(async move {
        let _subject_driver = subject_driver;
        crate::runtime_subject::supervise(make_subject, subject_store, agent_id,
            subject_live, cancel_subject).await
    });
    let mut subject_finished = false;
    let runtime_result: Result<()> = tokio::select! {
        _result = &mut subject_task => {
            subject_finished = true;
            match _result {
                Ok(Err(error)) => Err(error.into()),
                Err(error) => Err(error.into()),
                Ok(Ok(())) => Err(anyhow::anyhow!("subject stopped unexpectedly")),
            }
        }
        _result = ui.run() => _result,
        _result = tokio::signal::ctrl_c() => {
            match _result {
                Ok(()) => {
                    info!("shutdown signal; settling subject at a recoverable boundary");
                    Ok(())
                }
                Err(error) => Err(error.into()),
            }
        }
        _ = cancel.cancelled() => Err(anyhow::anyhow!("runtime stopped after connector failure; reopen for recovery")),
    };
    live.set_wired(false);
    // Signal expected delivery shutdown before closing the receiver.
    // Durable consumers retain their separate settlement lifetime.
    cancel.cancel();
    native_child.request_stop();
    if let Some(inference) = &state.inference { inference.request_stop(); }
    subject_inbox.close().await;
    if !subject_finished {
        // The loop leaves its wait without abandoning the broker's receipt.
        // A slow local hook/worker remains owned: timeout is only diagnostic.
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
                match store.load_subject(agent_id) {
                    Ok((_, step)) => tracing::warn!(step_id=%step.step_id, state=?step.state, request_id=?step.active_request_id,
                        "subject shutdown still pending after 15s; retaining actual worker ownership; remote outcome is not proven stopped"),
                    Err(error) => tracing::warn!(%error,
                        "subject shutdown still pending after 15s and durable state could not be read; retaining actual worker ownership"),
                }
                subject_task.await??;
            }
        }
    }
    // S1 has settled or left a durable unresolved step. Do not drop the
    // consumers first. Their unfinished work remains queryable on reopen.
    consumers.stop(&authority_runtime).await?;
    runtime_result?;
    info!("subject and durable consumers settled; joining runtime services");
    Ok(())
}

/// Keep durable ingress, configuration and diagnostics available without
/// claiming a thinking primary. No dispatcher or model request is started.
struct DeterministicUi {
    port: u16,
    store: Arc<shenko_store::Store>,
    data_dir: PathBuf,
    agent_id: shenko_types::ids::EntityId,
    runtime_instance: shenko_types::ids::EntityId,
    model_profile: shenko_seed::ModelProfile,
    model_health: Arc<crate::runtime_model_health::RuntimeModelHealth>,
    cancel: tokio_util::sync::CancellationToken,
    owner: shenko_ui::owner::OwnerState,
    channels: runtime_channels::ChannelConfig,
    reason: String,
    file_mounts: Arc<shenko_ui::files::FileMounts>,
}

async fn run_deterministic_ui(
    supervisor: &shenko_kernel::Supervisor, input: DeterministicUi,
) -> Result<()> {
    let DeterministicUi { port, store, data_dir, agent_id, runtime_instance,
        model_profile, model_health, cancel, owner, channels, reason, file_mounts } = input;
    let subject_inbox = Arc::new(shenko_subject::inbox::Inbox::new(1024));
    // Channel-owned native definitions use the ordinary lifecycle registry;
    // absence of a dispatch consumer prevents new effect execution here.
    let _native = supervisor.spawn(shenko_subject::NativeChildPlugin,
        (Arc::clone(&store), shenko_types::ids::RuntimeGeneration::default())).await?;
    let _channels = runtime_channels::install(runtime_channels::ChannelInstall {
        supervisor, store: &store, data_dir: &data_dir, agent_id,
        subject_inbox: &subject_inbox, self_knowledge: None,
        consumer_cancel: &cancel, config: channels,
    }).await?;
    let replication = node_sync::spawn_runtime(Arc::clone(&store), data_dir.clone(), cancel.clone());
    node_sync::open_node(&data_dir, &store)?.observe_runtime_roles(
        runtime_instance, vec![shenko_node::NodeRole::Recovery, shenko_node::NodeRole::Replica],
        Some(agent_id), "deterministic recovery and replication; no thought or effect dispatcher started".into(), None,
    )?;
    let ui = shenko_ui::server::UiServer::new(port)
        .with_inbox(subject_inbox.sender())
        .with_store(store)
        .with_file_mounts(file_mounts)
        .with_file_peers(file_peers(&data_dir).ok())
        .with_data_dir(data_dir)
        .with_owner(owner)
        .with_inference_status(model_profile.model, model_profile.configured);
    let live = ui.ws_state().live;
    live.set_subject_blocked(Some(reason));
    println!("Shenko UI: http://{}", ui.bind);
    println!("  Subject: capacity blocked; durable input and recovery remain available");
    let restored = async {
        loop {
            if model_health.is_eligible(runtime_instance) { break; }
            model_health.wait_for_change().await;
        }
    };
    let result = tokio::select! {
        _result = ui.run() => _result,
        _ = cancel.cancelled() => Err(anyhow::anyhow!("deterministic recovery stopped; retained inputs require recovery")),
        _ = restored => Err(crate::runtime_authority::AuthorityRecoveryRequired(
            "new actual model evidence permits primary selection".into()).into()),
        _result = tokio::signal::ctrl_c() => _result.map_err(anyhow::Error::from),
    };
    cancel.cancel();
    live.set_wired(false);
    subject_inbox.close().await;
    replication.await?;
    result
}

/// The nodes this node syncs with, read without taking the node's writer
/// role, for the file view. An installation with no node database has none.
fn file_peers(data_dir: &std::path::Path) -> Result<Vec<shenko_ui::files::FilePeer>> {
    let Some(node) = shenko_node::Node::open_inspect(&data_dir.join("node"))? else {
        return Ok(Vec::new());
    };
    Ok(node.sync_peers()?
        .into_iter()
        .map(|state| shenko_ui::files::FilePeer {
            node_id: state.peer.node_id.to_string(),
            ssh_target: state.peer.ssh_target,
            workspaces: state.peer.workspaces,
            remote_data_dir: state.peer.remote_data_dir,
            confirmed_at: state.confirmation.map(|confirmation| confirmation.confirmed_at.to_rfc3339()),
            failure: state.failure.map(|failure| failure.reason),
        })
        .collect())
}

#[cfg(test)]
mod file_peer_tests {
    #[test]
    fn unknown_peer_inventory_is_distinct_from_no_installation() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("shenko-file-peers-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir_all(root.join("node"))?;
        let absent = super::file_peers(&root);
        std::fs::write(root.join("node/node.sqlite3"), b"invalid node database")?;
        let unavailable = super::file_peers(&root);
        std::fs::remove_dir_all(root)?;
        assert!(absent?.is_empty());
        assert!(unavailable.is_err());
        Ok(())
    }
}
