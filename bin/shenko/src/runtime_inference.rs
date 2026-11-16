//! Context and inference composition after model-independent recovery is installed.
use std::{path::Path, sync::Arc};

use anyhow::Result;
use tracing::info;

use crate::{context_policy, inference_runtime, node_sync, update_runtime};
use crate::runtime_plugins::InstalledDefinitions;

pub(crate) struct InferenceServices {
    pub(crate) context_registry: Arc<shenko_context_policy::PolicyRegistry>,
    pub(crate) broker_handle: shenko_broker::broker::BrokerHandle,
    pub(crate) capabilities: Option<shenko_broker::request::ProviderCapabilities>,
}

/// The composed inference installation inputs, grouped so the installer
/// signature states its work rather than the composition root's arity.
pub(crate) struct InferenceInstall<'a> {
    pub(crate) supervisor: &'a shenko_kernel::Supervisor,
    pub(crate) store: &'a Arc<shenko_store::Store>,
    pub(crate) data_dir: &'a Path,
    pub(crate) agent_id: shenko_types::ids::EntityId,
    pub(crate) subject_inbox: &'a Arc<shenko_subject::inbox::Inbox>,
    pub(crate) cognition: &'a Arc<shenko_awareness_core::CognitionCatalog>,
    pub(crate) installed: &'a InstalledDefinitions,
    pub(crate) cancel: &'a tokio_util::sync::CancellationToken,
    pub(crate) consumer_cancel: &'a tokio_util::sync::CancellationToken,
}

/// `None` is an observed user stop during read-only metadata discovery. Runtime
/// failure cancellation remains an error, and neither path starts the broker.
pub(crate) async fn install(input: InferenceInstall<'_>) -> Result<Option<InferenceServices>> {
    let InferenceInstall {
        supervisor, store, data_dir, agent_id, subject_inbox,
        cognition, installed, cancel, consumer_cancel,
    } = input;
    let handoff_participants = &installed.handoff_participants;
    let self_knowledge = &installed.self_knowledge;
    // Start inference only after the model-independent recovery services.
    let (context_head, _) = store.load_subject(agent_id)?;
    let context_registry = Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
        shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(
                Arc::clone(store), shenko_types::ids::EntityId(context_head.branch_id.0),
            ),
        )),
    ));
    let _context_host = supervisor
        .spawn(shenko_context_policy::ContextPolicyHost, Arc::clone(&context_registry))
        .await?;
    // Prepared transient context contributions: producers (the PMA
    // intervention ledger, awareness perspectives, a plugin's own
    // notice) register here, and the request path collects what they
    // prepared. The registry holds no business content — it merges
    // by the kernel's one rule and records what was sent.
    let contribution_registry =
        Arc::new(shenko_context_policy::ContributionRegistry::new());
    let _contribution_host = supervisor
        .spawn(
            shenko_context_policy::ContributionHost,
            Arc::clone(&contribution_registry),
        )
        .await?;
    supervisor.spawn(
        shenko_memory::intervention_runtime::MemoryInterventionPlugin,
        shenko_memory::intervention_runtime::MemoryInterventionConfig {
            store: Arc::clone(store),
            agent_id,
            policy: Arc::new(shenko_memory::MemoryInterventionRegistry::new(Arc::new(
                shenko_memory::ModelMemoryInterventionPolicy,
            ))),
            failure_cancel: cancel.clone(),
        },
    ).await?;
    // The Node installation records are the installation authority for
    // the context strategies: a strategy runs only while its unit is
    // enabled, with the configuration that unit carries. The
    // preference order follows the installation order of the enabled
    // units until the configuration service carries a value.
    // The notebook and the explicit rollover request belong to this
    // branch's context chain, so they share its scope and live in the
    // store: a restarted process resumes the same journal, and a
    // request armed before it stopped is still pending afterwards.
    let context_scope = shenko_types::ids::EntityId(context_head.branch_id.0);
    let notes_journal: Arc<dyn shenko_context_policy::NotesJournal> =
        Arc::new(shenko_context_policy::StoreNotesJournal::new(
            Arc::clone(store),
            context_scope,
        ));
    let rollover_journal: Arc<dyn shenko_context_policy::RolloverJournal> =
        Arc::new(shenko_context_policy::StoreRolloverJournal::new(
            Arc::clone(store),
            context_scope,
        ));
    // The reminder answer is store-backed for the same reason: a
    // process that restarts inside the same committed window must not
    // repeat prompt material the model already received.
    let reminder_journal: Arc<dyn shenko_context_policy::ReminderJournal> =
        Arc::new(shenko_context_policy::StoreReminderJournal::new(
            Arc::clone(store),
            context_scope,
        ));
    context_registry
        .attach_rollover_journal(rollover_journal)
        .map_err(anyhow::Error::msg)?;
    context_registry
        .attach_reminder_journal(reminder_journal)
        .map_err(anyhow::Error::msg)?;
    let mut policy_node = node_sync::open_node(data_dir, store)?;
    let installed_policies = context_policy::reconcile_units(
        &mut policy_node,
        &data_dir.join("policy-units"),
    )?;
    let policy_node = Arc::new(parking_lot::Mutex::new(policy_node));
    let adopted = context_registry.apply_preference(
        context_policy::preference_order(&installed_policies),
    );
    info!(
        adopted = ?adopted.adopted,
        unknown = ?adopted.unknown,
        "context strategies installed from the unit records"
    );
    // The installation record is observed for the lifetime of this
    // runtime instead of read once at boot: a strategy enabled,
    // disabled, replaced or reconfigured by another Node connection
    // starts or stops here, and the notebook definition a strategy owns
    // is withdrawn with it. A policy that boots enabled is installed by
    // the same reconciliation, so there is one installation path.
    let _installed_policies = context_policy::spawn_installed_policies(
        supervisor,
        context_policy::PolicyHosts {
            registry: Arc::clone(&context_registry),
            notes: Arc::clone(&notes_journal),
            store: Arc::clone(store),
            node: Arc::clone(&policy_node),
        },
    )
    .await?;
    installed.config_service.register(Arc::new(
        crate::policy_config::ContextPreferenceNamespace::open(
            Arc::clone(store), Arc::clone(&context_registry),
        )?,
    ));
    let (broker, broker_handle) = shenko_broker::broker::Broker::new()?;
    inference_runtime::install_runtime_error_sink(&broker_handle, Arc::clone(store));
    handoff_participants.register(Arc::new(update_runtime::InferenceDomain(broker_handle.clone())));
    // Optional quota policy install: the file holds one
    // serialized QuotaPolicy; a recorded policy makes every inference
    // request pass durable admission before dispatch. Unreadable or
    // invalid content fails startup loudly rather than silently
    // running unpriced.
    if let Ok(path) = std::env::var("SHENKO_QUOTA_POLICY") {
        let text = std::fs::read_to_string(&path)
            .map_err(|error| anyhow::anyhow!("SHENKO_QUOTA_POLICY unreadable: {error}"))?;
        let policy: shenko_quota::QuotaPolicy = serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("SHENKO_QUOTA_POLICY invalid: {error}"))?;
        let receipt = store.record_quota_policy(&policy, chrono::Utc::now())?;
        info!(policy_id=%policy.policy_id, revision=policy.revision,
            commit=%receipt.commit_ref, "quota policy recorded");
    }
    // Provider quota observers: the local
    // account configuration names the providers this node observes,
    // the real scope of each account and where its sources are read.
    // Each named provider installs its own observers; the runtime
    // reads the real payloads, records the observations and turns
    // significant changes into a bounded view the subject can read.
    // Without a configuration there is no observer and nothing is
    // collected — observation from ordinary traffic stays available
    // either way.
    let quota_runtime = shenko_subject::quota_runtime::QuotaRuntime::from_env(
        Arc::clone(store),
        agent_id,
        Some(subject_inbox.sender()),
    )?;
    if let Some(quota_runtime) = quota_runtime {
        info!(providers = ?quota_runtime.providers(), "quota observers registered");
        let quota_stop = consumer_cancel.clone();
        tokio::spawn(async move {
            Arc::new(quota_runtime).run(quota_stop).await;
        });
    }
    // This read-only observation has no admitted inference to settle. Drop its
    // HTTP future on stop, instead of delaying shutdown for the provider's
    // request deadline. Do not interrupt the surrounding plugin installation.
    let discovered = {
        use std::{future::Future, task::Poll};
        let mut cancelled = Box::pin(cancel.cancelled());
        let mut signal = Box::pin(tokio::signal::ctrl_c());
        let mut discovery = Box::pin(broker.discover_capabilities());
        let outcome = std::future::poll_fn(|cx| {
            if cancelled.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(anyhow::anyhow!(
                    "runtime stopped during model metadata discovery; reopen for recovery"
                )));
            }
            if let Poll::Ready(signal) = signal.as_mut().poll(cx) {
                return Poll::Ready(signal.map(|()| None).map_err(anyhow::Error::from));
            }
            discovery.as_mut().poll(cx).map(|result| Ok(Some(result)))
        })
        .await?;
        match outcome {
            None => {
                info!(
                    "shutdown signal during model metadata discovery; inference was not admitted"
                );
                cancel.cancel();
                return Ok(None);
            }
            Some(result) => result,
        }
    };
    let capabilities = match discovered {
        Ok(capabilities) => Some(capabilities),
        Err(error) if error.is::<shenko_broker::InvalidModelMetadata>() => {
            return Err(error.context("provider model metadata was invalid; inference was not admitted"));
        }
        Err(error) => {
            tracing::warn!(error=%error, "model metadata unavailable; automatic context compaction disabled");
            None
        }
    };
    let model_child_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_subject::ModelChildHandler::new(Arc::clone(store), broker_handle.clone(), supervisor.context().clone())?,
    );
    let _model_child_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, model_child_handler)
        .await?;
    supervisor.spawn(
        shenko_subject::intervention::PmaEvaluatePlugin,
        (Arc::clone(store), broker_handle.clone()),
    ).await?;
    // The measurement path of a cognition unit: a finite comparison of
    // the candidate against the same installation with the unit
    // switched off. It runs in throwaway sandboxes and its inference
    // goes through this broker, so the trial is metered like every
    // other inference.
    let cognition_trial_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::CognitionTrialHandler::new(
            Arc::clone(store),
            Arc::clone(cognition),
            broker_handle.clone(),
            data_dir.join("cognition-trials"),
        ));
    let _native_cognition_trial_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            cognition_trial_handler,
        )
        .await?;
    // ModelChildHandler reconstructed all outstanding original charges
    // before any subject/child inference can enter this broker.
    tokio::spawn(broker.run());
    // The broker and model-child registrations above joined the live
    // catalog; the self-knowledge snapshot is a registry view and the
    // re-render is idempotent, so refresh it once more here.
    self_knowledge.install_agent_capabilities(&supervisor.runtime.agent_capabilities());
    Ok(Some(InferenceServices { context_registry, broker_handle, capabilities }))
}
