//! Cognition installation and native readers over the existing Node authority.
use std::{path::Path, sync::Arc};

use anyhow::Result;
use tracing::info;

use crate::{compat, node_sync, obelisk};

pub(crate) async fn install(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::Store>,
    data_dir: &Path,
    native_child: &Arc<shenko_subject::NativeChildService>,
    self_knowledge: &Arc<shenko_awareness_core::SelfKnowledgeProvider>,
) -> Result<Arc<shenko_awareness_core::CognitionCatalog>> {
    // Cognition units are installed capability, not kernel business
    // logic: perception, awareness, Skill and code share one
    // installation authority — the Node's unit store. The
    // catalog is a read model over it; the shipped seeds plus any
    // authored packages under data_dir/cognition install through it.
    // Nothing here dispatches on keywords; every installed unit's
    // short card travels as bounded advisory text and the model reads
    // the body it actually wants at an exact version.
    let cognition_node = Arc::new(parking_lot::Mutex::new(node_sync::open_node(
        data_dir,
        store,
    )?));
    let cognition = Arc::new(shenko_awareness_core::CognitionCatalog::new(
        Arc::clone(&cognition_node),
    ));
    let cognition_root = shenko_awareness_core::cognition_package_root(data_dir);
    // A shipped seed is bootstrap material, not authority over this
    // installation. A unit that is already installed keeps its own
    // version, its enable state and its observations.
    let installed_units: std::collections::HashSet<String> = cognition
        .units()?
        .into_iter()
        .map(|unit| unit.unit_id)
        .collect();
    for package in shenko_awareness_core::seed_packages()? {
        if installed_units.contains(&package.unit_id) {
            info!(unit = %package.unit_id, revision = %package.revision, "cognition seed already installed; keeping the installation's own version");
            continue;
        }
        cognition.install(&package, &cognition_root)?;
        info!(unit = %package.unit_id, revision = %package.revision, "cognition seed installed");
    }
    for unit in shenko_awareness_core::install_dir(&cognition, &cognition_root)? {
        info!(unit = %unit, "cognition unit installed");
    }
    // Legacy location kept for one release: packages authored under
    // data_dir/awareness install into the same authority.
    let legacy_awareness = data_dir.join("awareness");
    if legacy_awareness.is_dir() {
        for unit in shenko_awareness_core::install_dir(&cognition, &legacy_awareness)? {
            info!(unit = %unit, "legacy awareness unit installed");
        }
    }
    // One native generation owns recurring passive acquisition and archival.
    // Readiness means its first finite batch completed; a failed source is
    // disclosed as Unavailable, independently of installation read failures.
    let _perception_owner = supervisor.spawn(
        shenko_exec_host::RuntimePerceptionPlugin, Arc::clone(&cognition),
    ).await?;
    // Publish the one catalog so any plugin — including a finite
    // child's handler — discloses the same installed units. Publishing
    // grants nothing: a reader still has to be able to make the call
    // the index names, and the index says so when it cannot.
    let _cognition_catalog_plugin = supervisor
        .spawn(
            shenko_awareness_core::CognitionCatalogHost,
            Arc::clone(&cognition),
        )
        .await?;
    // Optional ecosystem adapters. MCP and Agent Skills are two
    // independent units over the same installation authority: each is
    // resident only while its own unit is enabled, and neither brings
    // a third-party service, Skill, dependency or account with it.
    let _compat_plugins = compat::spawn_adapters(
        supervisor,
        Arc::clone(store),
        Arc::clone(&cognition_node),
        data_dir,
    )
    .await?;
    let _obelisk_plugin = obelisk::spawn(supervisor, Arc::clone(store), Arc::clone(&cognition_node), data_dir).await?;
    // Exact-version unit reads and authored unit writes are native
    // children on the same Cordis root: the subject can inspect what
    // it has and write down what it actually learned, with the
    // judgement and evidence recorded as its own durable facts.
    let cognition_read_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::CognitionReadHandler::new(
            Arc::clone(store),
            Arc::clone(&cognition),
        ));
    let _native_cognition_read_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            cognition_read_handler,
        )
        .await?;
    let cognition_write_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::CognitionWriteHandler::new(
            Arc::clone(store),
            Arc::clone(&cognition),
            cognition_root.clone(),
            Some(Arc::clone(self_knowledge)),
        ));
    let _native_cognition_write_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            cognition_write_handler,
        )
        .await?;
    // A version that is already installed is judged here instead of
    // being written again: after a sandbox self-test the subject cites
    // the trial attempt of the exact artifacts it measured, and no
    // second artifact is installed under the same revision.
    let cognition_judge_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::CognitionJudgeHandler::new(
            Arc::clone(store),
            Arc::clone(&cognition),
            Some(Arc::clone(self_knowledge)),
        ));
    let _native_cognition_judge_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            cognition_judge_handler,
        )
        .await?;
    // The discovery index and the registered evidence of one unit are
    // reads over the same installation authority: the model pages
    // through the cards and cites what this installation actually
    // recorded.
    let cognition_list_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::CognitionQueryHandler::list(
            Arc::clone(store),
            Arc::clone(&cognition),
        ));
    let _native_cognition_list_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            cognition_list_handler,
        )
        .await?;
    let cognition_evidence_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::CognitionQueryHandler::evidence(
            Arc::clone(store),
            Arc::clone(&cognition),
        ));
    let _native_cognition_evidence_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            cognition_evidence_handler,
        )
        .await?;
    // Record what actually registered so self-knowledge answers from
    // the real registries instead of a stale product feature list.
    self_knowledge.install_runtime_capabilities(
        &native_child.installed_definitions(),
        &cognition.units()?,
    );
    // The live agent-visible capability catalog names what this
    // process currently offers; the snapshot is a registry view, not
    // a product list. Providers that register later (e.g. the broker)
    // refresh the view with a second, idempotent call below.
    self_knowledge.install_agent_capabilities(&supervisor.runtime.agent_capabilities());
    // the build manifest and the source index are runtime
    // facts of this artifact, not of the repository's main branch.
    self_knowledge.install_manifest_resource();
    if let Ok(source_root) = std::env::var("SHENKO_SOURCE_ROOT") {
        let revision = option_env!("SHENKO_SOURCE_REVISION").unwrap_or("unbound");
        match self_knowledge.install_source_index(Path::new(&source_root), revision) {
            Ok(indexed) => info!(indexed, source_root, "self-knowledge source index installed"),
            Err(error) => tracing::warn!(
                error = %error,
                source_root,
                "source index unavailable; self-knowledge reports the manifest only"
            ),
        }
    }
    Ok(cognition)
}
