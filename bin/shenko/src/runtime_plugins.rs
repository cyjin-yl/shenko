//! Native definition composition shared by live startup and capability discovery.
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::{document, node_sync, runtime_sessions, update_control, update_runtime};

/// The native definitions this runtime installs, and the self-knowledge
/// provider they feed.
pub(crate) struct InstalledDefinitions {
    pub(crate) native_child: Arc<shenko_subject::NativeChildService>,
    pub(crate) self_knowledge: Arc<shenko_awareness_core::SelfKnowledgeProvider>,
    pub(crate) handoff_participants: Arc<shenko_update::handoff::HandoffParticipantRegistry>,
    pub(crate) artifact_services: Arc<shenko_ui::playground_backend::ArtifactServiceController>,
    /// The one configuration service: the panel routes and the subject's
    /// `config.manage` tool share it, so a change made one way is the state
    /// the other observes.
    pub(crate) config_service: Arc<shenko_ui::config_service::ConfigService>,
    /// The one Playground instance: the subject's `playground.manage` child
    /// and the Web UI's artifact routes write the same lifecycle.
    pub(crate) playground_state: Arc<shenko_ui::playground::PlaygroundState>,
}

/// The media services one process publishes: the typed ingress every provider
/// (and the `/api/media` routes) admits through, and the live runtime the
/// session, recording, speech and speaker-identity engines hang off.
pub(crate) struct MediaServices {
    pub(crate) ingress: Arc<shenko_media::MediaIngress>,
    pub(crate) runtime: Arc<shenko_media::runtime::MediaRuntime>,
}

/// Construct and publish the real media services on the shared Cordis root —
///
///
/// This is the single construction path, and it runs before the native
/// definitions that bind to it. The running agent passes its live subject
/// inbox so an admitted media fact wakes the same subject; capability
/// discovery publishes the same pair so a definition it reports as installed
/// really has the service behind it. Nothing here is a stub: the runtime holds
/// the engine bindings loaded from `<data_dir>/media/providers.json`, and an
/// absent file means no engine is configured — which the handlers report as a
/// typed refusal at call time rather than as a fake capability.
pub(crate) async fn spawn_media_services(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::store::Store>,
    data_dir: &std::path::Path,
    agent_id: shenko_types::ids::EntityId,
    inbox: tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>,
) -> Result<MediaServices> {
    let _media_ingress_plugin = supervisor
        .spawn(
            shenko_media::MediaIngressPlugin,
            (Arc::clone(store), agent_id, inbox),
        )
        .await?;
    // Engine bindings are configuration, not model input: a malformed binding
    // is a typed error here instead of a silently engine-less runtime.
    let media_providers =
        shenko_media::runtime::MediaProviders::load(data_dir, Arc::clone(&store.cas))?;
    let _media_runtime_plugin = supervisor
        .spawn(
            shenko_media::MediaRuntimePlugin,
            (
                supervisor
                    .context()
                    .try_service::<shenko_media::MediaIngress>()?,
                Arc::clone(store),
                media_providers,
            ),
        )
        .await?;
    Ok(MediaServices {
        ingress: supervisor
            .context()
            .try_service::<shenko_media::MediaIngress>()?,
        runtime: supervisor
            .context()
            .try_service::<shenko_media::runtime::MediaRuntime>()?,
    })
}

/// Install the native task definitions of this runtime.
///
/// Every definition is a real Cordis plugin with a real generation lifetime:
/// it registers itself in the live plugin registry while it is installed and
/// withdraws that registration in the same teardown. The function is the one
/// installation path — the running agent and the discovery command install
/// exactly the same definitions, so the advertised capabilities are the ones
/// that actually exist.
pub(crate) async fn install_native_definitions(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::store::Store>,
    data_dir: &std::path::Path,
    wasi_mounts: Vec<shenko_wasm::MountSpec>,
    media: &MediaServices,
) -> Result<InstalledDefinitions> {
    let handoff_participants = Arc::new(
        shenko_update::handoff::HandoffParticipantRegistry::new(),
    );
    handoff_participants.bind_subject_driver(false);
    // Native finite children use the same Cordis root and persistent
    // ledger. Fixed native definitions retain their original handlers;
    // the finite model handler is installed with the shared broker below.
    let _native_child_plugin = supervisor
        .spawn(
            shenko_subject::NativeChildPlugin,
            (
                Arc::clone(store),
                shenko_types::ids::RuntimeGeneration::default(),
            ),
        )
        .await?;
    let printf_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HostPrintfHandler::new(Arc::clone(store)));
    let _native_printf_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, printf_handler)
        .await?;
    let history_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HistoryHandler::new(Arc::clone(store)));
    let _native_history_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, history_handler)
        .await?;
    document::install(supervisor, Arc::clone(store), node_sync::open_node(data_dir, store)?).await?;
    // Reachability is observed, not assumed: one bounded probe of a declared
    // endpoint with per-stage measured timings and no persistent state.
    let net_probe_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NetProbeHandler::new(Arc::clone(store)));
    let _native_net_probe_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, net_probe_handler)
        .await?;
    let memory_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::MemoryHandler::new(Arc::clone(store)));
    let _native_memory_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, memory_handler)
        .await?;
    for handler in shenko_exec_host::research_handlers(Arc::clone(store)) {
        supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    }
    // Runtime-error awareness: errors.read exposes the
    // subject's own committed runtime_error facts; issue.submit writes
    // subject-authored drafts into the local issues outbox only.
    let errors_read_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::ErrorsReadHandler::new(Arc::clone(store)));
    let _native_errors_read_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, errors_read_handler)
        .await?;
    let issue_submit_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_exec_host::IssueSubmitHandler::new(
            Arc::clone(store),
            data_dir.join("issues"),
        ),
    );
    let _native_issue_submit_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, issue_submit_handler)
        .await?;
    // Person-directory operations share the live native registry; owner-set
    // admin grants remain outside the subject's task surface.
    let person_directory_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::PersonDirectoryHandler::new(Arc::clone(store)));
    let _native_person_directory_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, person_directory_handler)
        .await?;
    let media_read_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_media::MediaReadHandler::new(Arc::clone(store)));
    let _media_read_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, media_read_handler)
        .await?;
    // Speaker identification of one already-sealed segment. The handler binds
    // to the SAME live media runtime the UI routes and provider plugins use,
    // which is why the media services are constructed before this install.
    let media_identify_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_media::MediaIdentifyHandler::new(
            Arc::clone(store),
            Arc::clone(&media.runtime),
        ),
    );
    let _media_identify_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, media_identify_handler)
        .await?;
    // Built-in, replaceable self-explanation, R124. The
    // provider is seeded from compiled-in material now; the real
    // `self://runtime/capabilities` snapshot is installed below once
    // the native and awareness registries are populated.
    let self_knowledge = Arc::new(shenko_awareness_core::SelfKnowledgeProvider::new(
        option_env!("SHENKO_SOURCE_REVISION").unwrap_or("unbound"),
        "0.6.2",
    ));
    let self_knowledge_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::SelfKnowledgeHandler::new(
            Arc::clone(store),
            Arc::clone(&self_knowledge),
        ));
    let _native_self_knowledge_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            self_knowledge_handler,
        )
        .await?;
    let artifact_services = Arc::new(
        shenko_ui::playground_backend::ArtifactServiceController::new(
            Arc::clone(store),
            data_dir.join("playground"),
        ),
    );
    handoff_participants.register(Arc::new(update_runtime::PlaygroundServices(
        Arc::clone(&artifact_services),
    )));
    let playground_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_ui::PlaygroundBackendHandler::new(Arc::clone(store), data_dir.join("playground"))
            .with_services(Arc::clone(&artifact_services)),
    );
    let _native_playground_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, playground_handler)
        .await?;
    // The subject's own hands on its works. One Playground
    // instance serves the subject and the Web UI, so a version the subject
    // writes is the version a human sees.
    let playground_state = Arc::new(
        shenko_ui::playground::PlaygroundState::open(data_dir.join("playground"), Arc::clone(store))?,
    );
    let playground_manage_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_ui::playground_manage::PlaygroundManageHandler::new(
            Arc::clone(store),
            Arc::clone(&playground_state),
        ),
    );
    let _native_playground_manage_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            playground_manage_handler,
        )
        .await?;
    // Configuration through conversation, R133. The handler runs
    // the same ConfigService the panel routes expose: one durable change
    // ledger, one set of consumer adoptions, no approval gate and no second
    // configuration store. Its built-in namespaces are registered by the
    // service itself, and every plugin may add its own.
    let config_service = Arc::new(shenko_ui::config_service::ConfigService::new(
        Arc::clone(store),
    ));
    config_service.register(Arc::new(crate::policy_config::QuotaNamespace(Arc::clone(store))));
    let config_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_ui::ConfigHandler::new(
            Arc::clone(store),
            Arc::clone(&config_service),
        ));
    let _native_config_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, config_handler)
        .await?;
    runtime_sessions::install_device_sessions(supervisor, store, data_dir).await?;
    // One persisted workspace profile supplies four ordinary native plugins.
    // Its editor/root stay fixed for this process; a changed Node record is
    // adopted on the next start, alongside the next child's tool catalog.
    crate::workspace_tools::install(supervisor, store, data_dir).await?;
    // Real named-WASM-tool execution: wasi.exec resolves this
    // installation's own unit inventory and runs their verified
    // artifact bytes in the real WasmRuntime under the home mounts.
    let wasi_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::WasiExecHandler::new(
            Arc::clone(store),
            wasi_mounts,
            data_dir.to_path_buf(),
        ));
    let _native_wasi_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, wasi_handler)
        .await?;
    runtime_sessions::install_repl_sessions(supervisor, store, data_dir, &handoff_participants).await?;
    install_node_capability_reader(
        supervisor,
        Arc::clone(store),
        node_sync::open_node(data_dir, store)?,
    )
    .await?;
    install_node_selection_recorder(
        supervisor,
        Arc::clone(store),
        node_sync::open_node(data_dir, store)?,
    )
    .await?;
    runtime_sessions::install_seat_sessions(supervisor, store).await?;
    let native_child = supervisor
        .context()
        .try_service::<shenko_subject::NativeChildService>()?;
    playground_state.with_native(Arc::clone(&native_child));
    handoff_participants.register_admission_barrier(Arc::new(update_runtime::NativeWorkers(
        Arc::clone(&native_child),
    )));
    let agent_id = store.get_existing_agent_id()?.context("no initialized subject")?;
    let update_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_update::native::UpdateControlHandler::new(
            Arc::clone(store),
            data_dir.to_path_buf(),
            Arc::clone(&handoff_participants),
            Some(Arc::new(update_runtime::NodeAuthority {
                data_dir: data_dir.to_path_buf(),
                store: Arc::clone(store),
                agent_id,
            })),
            Arc::new(update_control::LiveControl(data_dir.to_path_buf())),
        ),
    );
    let _native_update_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, update_handler)
        .await?;
    Ok(InstalledDefinitions {
        native_child,
        self_knowledge,
        handoff_participants,
        artifact_services,
        config_service,
        playground_state,
    })
}


// Share the actual startup registration path with the lifecycle test. The
// native registry owns discovery and withdraws it with the Cordis generation.
pub(crate) async fn install_node_capability_reader(
    supervisor: &shenko_kernel::Supervisor,
    store: Arc<shenko_store::Store>,
    node: shenko_node::Node,
) -> Result<()> {
    let handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NodeCapabilitiesHandler::new(store, node));
    supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    Ok(())
}

// Independent installed contract: recording choices must never turn the
// observation-only v1 into a mutating API. Both reuse the same Node journal.
pub(crate) async fn install_node_selection_recorder(
    supervisor: &shenko_kernel::Supervisor,
    store: Arc<shenko_store::Store>,
    node: shenko_node::Node,
) -> Result<()> {
    let handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NodeSelectionHandler::new(store, node));
    supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    Ok(())
}

// The selected SSH helper handler shares the current native registry and
// generation lifetime; absence of explicit configuration leaves it uninstalled.
pub(crate) async fn install_native_ssh(
    supervisor: &shenko_kernel::Supervisor,
    store: Arc<shenko_store::Store>,
    node: shenko_node::Node,
    path: &std::path::Path,
) -> Result<()> {
    let handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NativeSshTaskHandler::from_file(store, node, path)?);
    supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    Ok(())
}
